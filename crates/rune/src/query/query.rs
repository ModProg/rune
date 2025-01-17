use core::mem::take;

use crate::no_std::borrow::Cow;
use crate::no_std::collections::{hash_map, BTreeMap, HashMap, HashSet, VecDeque};
use crate::no_std::prelude::*;
use crate::no_std::sync::Arc;

use crate::ast;
use crate::ast::{Span, Spanned};
use crate::compile::ir;
use crate::compile::meta;
use crate::compile::{
    self, CompileErrorKind, CompileVisitor, ComponentRef, Doc, ImportStep, IntoComponent, IrBudget,
    IrCompiler, IrInterpreter, Item, ItemBuf, ItemId, ItemMeta, Location, ModId, ModMeta, Names,
    Pool, Prelude, QueryErrorKind, SourceMeta, UnitBuilder, Visibility, WithSpan,
};
use crate::hir;
use crate::indexing::{self, Indexed};
use crate::macros::Storage;
use crate::parse::{Id, NonZeroId, Opaque, Resolve, ResolveContext};
use crate::query::{Build, BuildEntry, BuiltInMacro, ConstFn, Named, QueryPath, Used};
use crate::runtime::Call;
use crate::shared::{Consts, Gen, Items};
use crate::{Context, Hash, SourceId, Sources};

/// The permitted number of import recursions when constructing a path.
const IMPORT_RECURSION_LIMIT: usize = 128;

#[derive(Default)]
pub(crate) struct QueryInner {
    /// Resolved meta about every single item during a compilation.
    meta: HashMap<(ItemId, Hash), meta::Meta>,
    /// Build queue.
    queue: VecDeque<BuildEntry>,
    /// Indexed items that can be queried for, which will queue up for them to
    /// be compiled.
    indexed: BTreeMap<ItemId, Vec<indexing::Entry>>,
    /// Compiled constant functions.
    const_fns: HashMap<NonZeroId, Arc<ConstFn>>,
    /// Query paths.
    query_paths: HashMap<NonZeroId, QueryPath>,
    /// The result of internally resolved macros.
    internal_macros: HashMap<NonZeroId, BuiltInMacro>,
    /// Associated between `id` and `Item`. Use to look up items through
    /// `item_for` with an opaque id.
    ///
    /// These items are associated with AST elements, and encodoes the item path
    /// that the AST element was indexed.
    items: HashMap<NonZeroId, ItemMeta>,
    /// All available names.
    names: Names,
}

/// Query system of the rune compiler.
///
/// The basic mode of operation here is that you ask for an item, and the query
/// engine gives you the metadata for that item while queueing up any tasks that
/// need to be run to actually build that item and anything associated with it.
///
/// Note that this type has a lot of `pub(crate)` items. This is intentional.
/// Many components need to perform complex borrowing out of this type, meaning
/// its fields need to be visible (see the [resolve_context!] macro).
pub(crate) struct Query<'a> {
    /// The current unit being built.
    pub(crate) unit: &'a mut UnitBuilder,
    /// The prelude in effect.
    prelude: &'a Prelude,
    /// Cache of constants that have been expanded.
    pub(crate) consts: &'a mut Consts,
    /// Storage associated with the query.
    pub(crate) storage: &'a mut Storage,
    /// Sources available.
    pub(crate) sources: &'a mut Sources,
    /// Pool of allocates items and modules.
    pub(crate) pool: &'a mut Pool,
    /// Visitor for the compiler meta.
    pub(crate) visitor: &'a mut dyn CompileVisitor,
    /// Shared id generator.
    pub(crate) gen: &'a Gen,
    /// Inner state of the query engine.
    inner: &'a mut QueryInner,
}

impl<'a> Query<'a> {
    /// Construct a new compilation context.
    pub(crate) fn new(
        unit: &'a mut UnitBuilder,
        prelude: &'a Prelude,
        consts: &'a mut Consts,
        storage: &'a mut Storage,
        sources: &'a mut Sources,
        pool: &'a mut Pool,
        visitor: &'a mut dyn CompileVisitor,
        gen: &'a Gen,
        inner: &'a mut QueryInner,
    ) -> Self {
        Self {
            unit,
            prelude,
            consts,
            storage,
            sources,
            pool,
            visitor,
            gen,
            inner,
        }
    }

    /// Reborrow the query engine from a reference to `self`.
    pub(crate) fn borrow(&mut self) -> Query<'_> {
        Query {
            unit: self.unit,
            prelude: self.prelude,
            consts: self.consts,
            storage: self.storage,
            pool: self.pool,
            sources: self.sources,
            visitor: self.visitor,
            gen: self.gen,
            inner: self.inner,
        }
    }

    /// Get the next build entry from the build queue associated with the query
    /// engine.
    pub(crate) fn next_build_entry(&mut self) -> Option<BuildEntry> {
        self.inner.queue.pop_front()
    }

    /// Insert path information.
    pub(crate) fn insert_path(
        &mut self,
        module: ModId,
        impl_item: Option<ItemId>,
        item: &Item,
    ) -> NonZeroId {
        let item = self.pool.alloc_item(item);
        let id = self.gen.next();

        let old = self.inner.query_paths.insert(
            id,
            QueryPath {
                module,
                impl_item,
                item,
            },
        );

        debug_assert!(old.is_none(), "should use a unique identifier");
        id
    }

    /// Remove a reference to the given path by id.
    pub(crate) fn remove_path_by_id(&mut self, id: Id) {
        if let Some(id) = id.as_ref() {
            self.inner.query_paths.remove(id);
        }
    }

    /// Insert module and associated metadata.
    pub(crate) fn insert_mod(
        &mut self,
        items: &Items,
        location: Location,
        parent: ModId,
        visibility: Visibility,
        docs: &[Doc],
    ) -> compile::Result<ModId> {
        let item = self.insert_new_item(items, location, parent, visibility, docs)?;

        let query_mod = self.pool.alloc_module(ModMeta {
            location,
            item: item.item,
            visibility,
            parent: Some(parent),
        });

        self.index_and_build(indexing::Entry {
            item_meta: item,
            indexed: Indexed::Module,
        });
        Ok(query_mod)
    }

    /// Insert module and associated metadata.
    pub(crate) fn insert_root_mod(
        &mut self,
        source_id: SourceId,
        spanned: Span,
    ) -> compile::Result<ModId> {
        let query_mod = self.pool.alloc_module(ModMeta {
            location: Location::new(source_id, spanned),
            item: ItemId::default(),
            visibility: Visibility::Public,
            parent: None,
        });

        self.insert_name(ItemId::default());
        Ok(query_mod)
    }

    /// Inserts an item that *has* to be unique, else cause an error.
    ///
    /// This are not indexed and does not generate an ID, they're only visible
    /// in reverse lookup.
    pub(crate) fn insert_new_item(
        &mut self,
        items: &Items,
        location: Location,
        module: ModId,
        visibility: Visibility,
        docs: &[Doc],
    ) -> compile::Result<ItemMeta> {
        let id = items.id().with_span(location.span)?;
        let item = self.pool.alloc_item(&*items.item());
        self.insert_new_item_with(id, item, location, module, visibility, docs)
    }

    /// Insert the given compile meta.
    #[allow(clippy::result_large_err)]
    pub(crate) fn insert_meta(
        &mut self,
        meta: meta::Meta,
    ) -> Result<(), compile::error::MetaConflict> {
        self.visitor.register_meta(meta.as_meta_ref(self.pool));

        match self
            .inner
            .meta
            .entry((meta.item_meta.item, meta.parameters))
        {
            hash_map::Entry::Occupied(e) => {
                return Err(compile::error::MetaConflict {
                    current: meta.info(self.pool),
                    existing: e.get().info(self.pool),
                    parameters: meta.parameters,
                });
            }
            hash_map::Entry::Vacant(e) => {
                e.insert(meta);
            }
        }

        Ok(())
    }

    /// Insert a new item with the given newly allocated identifier and complete
    /// `Item`.
    fn insert_new_item_with(
        &mut self,
        id: NonZeroId,
        item: ItemId,
        location: Location,
        module: ModId,
        visibility: Visibility,
        docs: &[Doc],
    ) -> compile::Result<ItemMeta> {
        // Emit documentation comments for the given item.
        if !docs.is_empty() {
            let ctx = resolve_context!(self);

            for doc in docs {
                self.visitor.visit_doc_comment(
                    Location::new(location.source_id, doc.span),
                    self.pool.item(item),
                    self.pool.item_type_hash(item),
                    doc.doc_string.resolve(ctx)?.as_ref(),
                );
            }
        }

        let item_meta = ItemMeta {
            id: Id::new(id),
            location,
            item,
            module,
            visibility,
        };

        self.inner.items.insert(id, item_meta);
        Ok(item_meta)
    }

    /// Insert a new expanded internal macro.
    pub(crate) fn insert_new_builtin_macro(
        &mut self,
        internal_macro: BuiltInMacro,
    ) -> compile::Result<NonZeroId> {
        let id = self.gen.next();
        self.inner.internal_macros.insert(id, internal_macro);
        Ok(id)
    }

    /// Get the item for the given identifier.
    pub(crate) fn item_for<T>(&self, ast: T) -> compile::Result<ItemMeta>
    where
        T: Spanned + Opaque,
    {
        match ast.id().as_ref().and_then(|n| self.inner.items.get(n)) {
            Some(item_meta) => Ok(*item_meta),
            None => Err(compile::Error::new(
                ast.span(),
                QueryErrorKind::MissingId {
                    what: "item",
                    id: ast.id(),
                },
            )),
        }
    }

    /// Get the built-in macro matching the given ast.
    pub(crate) fn builtin_macro_for<T>(&self, ast: T) -> compile::Result<&BuiltInMacro>
    where
        T: Spanned + Opaque,
    {
        match ast
            .id()
            .as_ref()
            .and_then(|n| self.inner.internal_macros.get(n))
        {
            Some(internal_macro) => Ok(internal_macro),
            None => Err(compile::Error::new(
                ast.span(),
                QueryErrorKind::MissingId {
                    what: "builtin macro",
                    id: ast.id(),
                },
            )),
        }
    }

    /// Insert an item and return its Id.
    #[tracing::instrument(skip_all)]
    fn insert_const_fn(&mut self, item_meta: ItemMeta, ir_fn: ir::IrFn) -> NonZeroId {
        let id = self.gen.next();
        tracing::trace!(item = ?self.pool.item(item_meta.item), id = ?id);

        self.inner
            .const_fns
            .insert(id, Arc::new(ConstFn { item_meta, ir_fn }));

        id
    }

    /// Get the constant function associated with the opaque.
    pub(crate) fn const_fn_for<T>(&self, ast: T) -> compile::Result<Arc<ConstFn>>
    where
        T: Spanned + Opaque,
    {
        match ast.id().as_ref().and_then(|n| self.inner.const_fns.get(n)) {
            Some(const_fn) => Ok(const_fn.clone()),
            None => Err(compile::Error::new(
                ast.span(),
                QueryErrorKind::MissingId {
                    what: "constant function",
                    id: ast.id(),
                },
            )),
        }
    }

    /// Index the given entry. It is not allowed to overwrite other entries.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index(&mut self, entry: indexing::Entry) {
        tracing::trace!(item = ?entry.item_meta.item);

        self.insert_name(entry.item_meta.item);

        self.inner
            .indexed
            .entry(entry.item_meta.item)
            .or_default()
            .push(entry);
    }

    /// Same as `index`, but also queues the indexed entry up for building.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_and_build(&mut self, entry: indexing::Entry) {
        self.inner.queue.push_back(BuildEntry {
            item_meta: entry.item_meta,
            used: Used::Used,
            build: Build::Query,
        });

        self.index(entry);
    }

    /// Index a constant expression.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_const<T>(
        &mut self,
        item_meta: ItemMeta,
        value: &T,
        f: fn(&T, &mut IrCompiler) -> compile::Result<ir::Ir>,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        let mut c = IrCompiler {
            source_id: item_meta.location.source_id,
            q: self.borrow(),
        };
        let ir = f(value, &mut c)?;

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Const(indexing::Const {
                module: item_meta.module,
                ir,
            }),
        });

        Ok(())
    }

    /// Index a constant function.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_const_fn(
        &mut self,
        item_meta: ItemMeta,
        item_fn: Box<ast::ItemFn>,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::ConstFn(indexing::ConstFn {
                location: item_meta.location,
                item_fn,
            }),
        });

        Ok(())
    }

    /// Add a new enum item.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_enum(&mut self, item_meta: ItemMeta) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Enum,
        });

        Ok(())
    }

    /// Add a new struct item that can be queried.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_struct(
        &mut self,
        item_meta: ItemMeta,
        ast: Box<ast::ItemStruct>,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Struct(indexing::Struct { ast }),
        });

        Ok(())
    }

    /// Add a new variant item that can be queried.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_variant(
        &mut self,
        item_meta: ItemMeta,
        enum_id: Id,
        ast: ast::ItemVariant,
        index: usize,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Variant(indexing::Variant {
                enum_id,
                ast,
                index,
            }),
        });

        Ok(())
    }

    /// Add a new function that can be queried for.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_closure(
        &mut self,
        item_meta: ItemMeta,
        ast: Box<ast::ExprClosure>,
        captures: Arc<[String]>,
        call: Call,
        do_move: bool,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Closure(indexing::Closure {
                ast,
                captures,
                call,
                do_move,
            }),
        });

        Ok(())
    }

    /// Add a new async block.
    #[tracing::instrument(skip_all)]
    pub(crate) fn index_async_block(
        &mut self,
        item_meta: ItemMeta,
        ast: ast::Block,
        captures: Arc<[String]>,
        call: Call,
        do_move: bool,
    ) -> compile::Result<()> {
        tracing::trace!(item = ?self.pool.item(item_meta.item));

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::AsyncBlock(indexing::AsyncBlock {
                ast,
                captures,
                call,
                do_move,
            }),
        });

        Ok(())
    }

    /// Remove and queue up unused entries for building.
    ///
    /// Returns boolean indicating if any unused entries were queued up.
    #[tracing::instrument(skip_all)]
    pub(crate) fn queue_unused_entries(
        &mut self,
    ) -> compile::Result<bool, (SourceId, compile::Error)> {
        tracing::trace!("queue unused");

        let unused = self
            .inner
            .indexed
            .values()
            .flat_map(|entries| entries.iter())
            .map(|e| (e.item_meta.location, e.item_meta.item))
            .collect::<Vec<_>>();

        if unused.is_empty() {
            return Ok(false);
        }

        for (location, item) in unused {
            let _ = self
                .query_indexed_meta(location.span, item, Used::Unused)
                .map_err(|e| (location.source_id, e))?;
        }

        Ok(true)
    }

    /// Explicitly look for meta with the given item and hash.
    pub(crate) fn get_meta(&self, item: ItemId, hash: Hash) -> Option<&meta::Meta> {
        self.inner.meta.get(&(item, hash))
    }

    /// Query for the given meta by looking up the reverse of the specified
    /// item.
    #[tracing::instrument(skip(self, span, item), fields(item = ?self.pool.item(item)))]
    pub(crate) fn query_meta(
        &mut self,
        span: Span,
        item: ItemId,
        used: Used,
    ) -> compile::Result<Option<meta::Meta>> {
        if let Some(meta) = self.inner.meta.get(&(item, Hash::EMPTY)) {
            tracing::trace!(item = ?item, meta = ?meta, "cached");
            // Ensure that the given item is not indexed, cause if it is
            // `queue_unused_entries` might end up spinning indefinitely since
            // it will never be exhausted.
            debug_assert!(!self.inner.indexed.contains_key(&item));
            return Ok(Some(meta.clone()));
        }

        self.query_indexed_meta(span, item, used)
    }

    /// Only try and query for meta among items which have been indexed.
    fn query_indexed_meta(
        &mut self,
        span: Span,
        item: ItemId,
        used: Used,
    ) -> compile::Result<Option<meta::Meta>> {
        if let Some(entry) = self.remove_indexed(span, item)? {
            let meta = self.build_indexed_entry(span, entry, used)?;
            self.unit.insert_meta(span, &meta, self.pool)?;
            self.insert_meta(meta.clone()).with_span(span)?;
            tracing::trace!(item = ?item, meta = ?meta, "build");
            return Ok(Some(meta));
        }

        Ok(None)
    }

    /// Perform a path lookup on the current state of the unit.
    #[tracing::instrument(skip_all)]
    pub(crate) fn convert_path<'hir>(
        &mut self,
        context: &Context,
        path: &'hir hir::Path<'hir>,
    ) -> compile::Result<Named<'hir>> {
        let id = path.id();

        let qp = *id
            .as_ref()
            .and_then(|id| self.inner.query_paths.get(id))
            .ok_or_else(|| {
                compile::Error::new(path, QueryErrorKind::MissingId { what: "path", id })
            })?;

        let mut in_self_type = false;
        let mut local = None;

        let item = match (path.global, path.first) {
            (
                Some(..),
                hir::PathSegment {
                    kind: hir::PathSegmentKind::Ident(ident),
                    ..
                },
            ) => self
                .pool
                .alloc_item(ItemBuf::with_crate(ident.resolve(resolve_context!(self))?)),
            (Some(span), _) => {
                return Err(compile::Error::new(
                    span,
                    CompileErrorKind::UnsupportedGlobal,
                ));
            }
            (None, segment) => match segment.kind {
                hir::PathSegmentKind::Ident(ident) => {
                    if path.rest.is_empty() {
                        local = Some(ident);
                    }

                    self.convert_initial_path(context, qp.module, qp.item, ident)?
                }
                hir::PathSegmentKind::Super => self
                    .pool
                    .try_map_alloc(self.pool.module(qp.module).item, Item::parent)
                    .ok_or_else(compile::Error::unsupported_super(segment.span()))?,
                hir::PathSegmentKind::SelfType => {
                    let impl_item = qp.impl_item.ok_or_else(|| {
                        compile::Error::new(segment.span(), CompileErrorKind::UnsupportedSelfType)
                    })?;

                    in_self_type = true;
                    impl_item
                }
                hir::PathSegmentKind::SelfValue => self.pool.module(qp.module).item,
                hir::PathSegmentKind::Crate => ItemId::default(),
                hir::PathSegmentKind::Generics(..) => {
                    return Err(compile::Error::new(
                        segment.span(),
                        CompileErrorKind::UnsupportedGenerics,
                    ));
                }
            },
        };

        let mut item = self.pool.item(item).to_owned();
        let mut trailing = 0;
        let mut parameters = [None, None];

        let mut it = path.rest.iter();
        let mut parameters_it = parameters.iter_mut();

        for segment in it.by_ref() {
            match segment.kind {
                hir::PathSegmentKind::Ident(ident) => {
                    item.push(ident.resolve(resolve_context!(self))?);
                }
                hir::PathSegmentKind::Super => {
                    if in_self_type {
                        return Err(compile::Error::new(
                            segment.span(),
                            CompileErrorKind::UnsupportedSuperInSelfType,
                        ));
                    }

                    item.pop()
                        .ok_or_else(compile::Error::unsupported_super(segment.span()))?;
                }
                hir::PathSegmentKind::Generics(arguments) => {
                    let Some(p) = parameters_it.next() else {
                        return Err(compile::Error::new(
                            segment,
                            CompileErrorKind::UnsupportedGenerics,
                        ));
                    };

                    trailing += 1;
                    *p = Some((segment.span(), arguments));
                    break;
                }
                _ => {
                    return Err(compile::Error::new(
                        segment.span(),
                        CompileErrorKind::ExpectedLeadingPathSegment,
                    ));
                }
            }
        }

        // Consume remaining generics, possibly interleaved with identifiers.
        while let Some(segment) = it.next() {
            let hir::PathSegmentKind::Ident(ident) = segment.kind else {
                return Err(compile::Error::new(
                    segment.span(),
                    CompileErrorKind::UnsupportedAfterGeneric,
                ));
            };

            trailing += 1;
            item.push(ident.resolve(resolve_context!(self))?);

            let Some(p) = parameters_it.next() else {
                return Err(compile::Error::new(
                    segment,
                    CompileErrorKind::UnsupportedGenerics,
                ));
            };

            let Some(hir::PathSegmentKind::Generics(arguments)) = it.clone().next().map(|p| p.kind) else {
                continue;
            };

            *p = Some((segment.span(), arguments));
            it.next();
        }

        let span = path.span();

        let local = match local {
            Some(local) => Some(local.resolve(resolve_context!(self))?.into()),
            None => None,
        };

        let item = self.pool.alloc_item(item);

        if let Some(new) = self.import(span, qp.module, item, Used::Used)? {
            return Ok(Named {
                local,
                item: new,
                trailing,
                parameters,
            });
        }

        Ok(Named {
            local,
            item,
            trailing,
            parameters,
        })
    }

    /// Declare a new import.
    #[tracing::instrument(skip_all)]
    pub(crate) fn insert_import(
        &mut self,
        source_id: SourceId,
        span: Span,
        module: ModId,
        visibility: Visibility,
        at: ItemBuf,
        target: ItemBuf,
        alias: Option<ast::Ident>,
        wildcard: bool,
    ) -> compile::Result<()> {
        tracing::trace!(at = ?at, target = ?target);

        let alias = match alias {
            Some(alias) => Some(alias.resolve(resolve_context!(self))?),
            None => None,
        };

        let last = alias
            .as_ref()
            .map(IntoComponent::as_component_ref)
            .or_else(|| target.last())
            .ok_or_else(|| compile::Error::new(span, QueryErrorKind::LastUseComponent))?;

        let item = self.pool.alloc_item(at.extended(last));
        let target = self.pool.alloc_item(target);
        let location = Location::new(source_id, span);

        let entry = meta::Import {
            location,
            target,
            module,
        };

        let id = self.gen.next();
        let item_meta = self.insert_new_item_with(id, item, location, module, visibility, &[])?;

        // toplevel public uses are re-exported.
        if item_meta.is_public(self.pool) {
            self.inner.queue.push_back(BuildEntry {
                item_meta,
                build: Build::ReExport,
                used: Used::Used,
            });
        }

        self.index(indexing::Entry {
            item_meta,
            indexed: Indexed::Import(indexing::Import { wildcard, entry }),
        });

        Ok(())
    }

    /// Check if unit contains the given name by prefix.
    pub(crate) fn contains_prefix(&self, item: &Item) -> bool {
        self.inner.names.contains_prefix(item)
    }

    /// Iterate over known child components of the given name.
    pub(crate) fn iter_components<'it, I: 'it>(
        &'it self,
        iter: I,
    ) -> impl Iterator<Item = ComponentRef<'it>> + 'it
    where
        I: IntoIterator,
        I::Item: IntoComponent,
    {
        self.inner.names.iter_components(iter)
    }

    /// Get the given import by name.
    #[tracing::instrument(skip(self, span, module))]
    pub(crate) fn import(
        &mut self,
        span: Span,
        mut module: ModId,
        item: ItemId,
        used: Used,
    ) -> compile::Result<Option<ItemId>> {
        let mut visited = HashSet::<ItemId>::new();
        let mut path = Vec::new();
        let mut item = self.pool.item(item).to_owned();
        let mut any_matched = false;

        let mut count = 0usize;

        'outer: loop {
            if count > IMPORT_RECURSION_LIMIT {
                return Err(compile::Error::new(
                    span,
                    QueryErrorKind::ImportRecursionLimit { count, path },
                ));
            }

            count += 1;

            let mut cur = ItemBuf::new();
            let mut it = item.iter();

            while let Some(c) = it.next() {
                cur.push(c);
                let cur = self.pool.alloc_item(&cur);

                let update = self.import_step(span, module, cur, used, &mut path)?;

                let update = match update {
                    Some(update) => update,
                    None => continue,
                };

                path.push(ImportStep {
                    location: update.location,
                    item: self.pool.item(update.target).to_owned(),
                });

                if !visited.insert(self.pool.alloc_item(&item)) {
                    return Err(compile::Error::new(
                        span,
                        QueryErrorKind::ImportCycle { path },
                    ));
                }

                module = update.module;
                item = self.pool.item(update.target).join(it);
                any_matched = true;
                continue 'outer;
            }

            break;
        }

        if any_matched {
            return Ok(Some(self.pool.alloc_item(item)));
        }

        Ok(None)
    }

    /// Inner import implementation that doesn't walk the imported name.
    #[tracing::instrument(skip(self, span, module, path))]
    fn import_step(
        &mut self,
        span: Span,
        module: ModId,
        item: ItemId,
        used: Used,
        path: &mut Vec<ImportStep>,
    ) -> compile::Result<Option<meta::Import>> {
        // already resolved query.
        if let Some(meta) = self.inner.meta.get(&(item, Hash::EMPTY)) {
            return Ok(match meta.kind {
                meta::Kind::Import(import) => Some(import),
                _ => None,
            });
        }

        // resolve query.
        let entry = match self.remove_indexed(span, item)? {
            Some(entry) => entry,
            _ => return Ok(None),
        };

        self.check_access_to(
            span,
            module,
            item,
            entry.item_meta.module,
            entry.item_meta.location,
            entry.item_meta.visibility,
            path,
        )?;

        let import = match entry.indexed {
            Indexed::Import(import) => import.entry,
            indexed => {
                self.import_indexed(span, entry.item_meta, indexed, used)?;
                return Ok(None);
            }
        };

        let meta = meta::Meta {
            context: false,
            hash: self.pool.item_type_hash(entry.item_meta.item),
            item_meta: entry.item_meta,
            kind: meta::Kind::Import(import),
            source: None,
            parameters: Hash::EMPTY,
        };

        self.insert_meta(meta).with_span(span)?;
        Ok(Some(import))
    }

    /// Build a single, indexed entry and return its metadata.
    fn build_indexed_entry(
        &mut self,
        span: Span,
        entry: indexing::Entry,
        used: Used,
    ) -> compile::Result<meta::Meta> {
        /// Convert AST fields into meta fields.
        fn convert_fields(
            ctx: ResolveContext<'_>,
            body: ast::Fields,
        ) -> compile::Result<meta::Fields> {
            Ok(match body {
                ast::Fields::Empty => meta::Fields::Empty,
                ast::Fields::Unnamed(tuple) => meta::Fields::Unnamed(tuple.len()),
                ast::Fields::Named(st) => {
                    let mut fields = HashSet::new();

                    for (ast::Field { name, .. }, _) in st {
                        let name = name.resolve(ctx)?;
                        fields.insert(name.into());
                    }

                    meta::Fields::Named(meta::FieldsNamed { fields })
                }
            })
        }

        let indexing::Entry { item_meta, indexed } = entry;

        let kind = match indexed {
            Indexed::Enum => meta::Kind::Enum {
                parameters: Hash::EMPTY,
            },
            Indexed::Variant(variant) => {
                let enum_ = self.item_for((span, variant.enum_id))?;

                // Ensure that the enum is being built and marked as used.
                let Some(enum_meta) = self.query_meta(span, enum_.item, Default::default())? else {
                    return Err(compile::Error::msg(span, format_args!("Missing enum by {:?}", variant.enum_id)));
                };

                meta::Kind::Variant {
                    enum_hash: enum_meta.hash,
                    index: variant.index,
                    fields: convert_fields(resolve_context!(self), variant.ast.body)?,
                    constructor: None,
                }
            }
            Indexed::Struct(st) => meta::Kind::Struct {
                fields: convert_fields(resolve_context!(self), st.ast.body)?,
                constructor: None,
                parameters: Hash::EMPTY,
            },
            Indexed::Function(f) => {
                let kind = meta::Kind::Function {
                    is_test: f.is_test,
                    is_bench: f.is_bench,
                    signature: meta::Signature {
                        #[cfg(feature = "doc")]
                        is_async: f.ast.async_token.is_some(),
                        #[cfg(feature = "doc")]
                        args: Some(f.ast.args.len()),
                        #[cfg(feature = "doc")]
                        return_type: None,
                        #[cfg(feature = "doc")]
                        argument_types: Box::from([]),
                    },
                    parameters: Hash::EMPTY,
                };

                self.inner.queue.push_back(BuildEntry {
                    item_meta,
                    build: Build::Function(f),
                    used,
                });

                kind
            }
            Indexed::InstanceFunction(f) => {
                let name: Cow<str> = Cow::Owned(f.ast.name.resolve(resolve_context!(self))?.into());

                let kind = meta::Kind::AssociatedFunction {
                    kind: meta::AssociatedKind::Instance(name),
                    signature: meta::Signature {
                        #[cfg(feature = "doc")]
                        is_async: f.ast.async_token.is_some(),
                        #[cfg(feature = "doc")]
                        args: Some(f.ast.args.len()),
                        #[cfg(feature = "doc")]
                        return_type: None,
                        #[cfg(feature = "doc")]
                        argument_types: Box::from([]),
                    },
                    parameters: Hash::EMPTY,
                    #[cfg(feature = "doc")]
                    container: self.pool.item_type_hash(f.impl_item),
                    #[cfg(feature = "doc")]
                    parameter_types: Vec::new(),
                };

                self.inner.queue.push_back(BuildEntry {
                    item_meta,
                    build: Build::InstanceFunction(f),
                    used,
                });

                kind
            }
            Indexed::Closure(c) => {
                let captures = c.captures.clone();
                let do_move = c.do_move;

                self.inner.queue.push_back(BuildEntry {
                    item_meta,
                    build: Build::Closure(c),
                    used,
                });

                meta::Kind::Closure { captures, do_move }
            }
            Indexed::AsyncBlock(b) => {
                let captures = b.captures.clone();
                let do_move = b.do_move;

                self.inner.queue.push_back(BuildEntry {
                    item_meta,
                    build: Build::AsyncBlock(b),
                    used,
                });

                meta::Kind::AsyncBlock { captures, do_move }
            }
            Indexed::Const(c) => {
                let mut const_compiler = IrInterpreter {
                    budget: IrBudget::new(1_000_000),
                    scopes: Default::default(),
                    module: c.module,
                    item: item_meta.item,
                    q: self.borrow(),
                };

                let const_value = const_compiler.eval_const(&c.ir, used)?;

                if used.is_unused() {
                    self.inner.queue.push_back(BuildEntry {
                        item_meta,
                        build: Build::Unused,
                        used,
                    });
                }

                meta::Kind::Const { const_value }
            }
            Indexed::ConstFn(c) => {
                let ir_fn = {
                    // TODO: avoid this arena?
                    let arena = crate::hir::Arena::new();
                    let ctx = crate::hir::lowering::Ctx::new(&arena, self.borrow());
                    let hir = crate::hir::lowering::item_fn(&ctx, &c.item_fn)?;
                    let mut c = IrCompiler {
                        source_id: c.location.source_id,
                        q: self.borrow(),
                    };
                    ir::IrFn::compile_ast(&hir, &mut c)?
                };

                let id = self.insert_const_fn(item_meta, ir_fn);

                if used.is_unused() {
                    self.inner.queue.push_back(BuildEntry {
                        item_meta,
                        build: Build::Unused,
                        used,
                    });
                }

                meta::Kind::ConstFn { id: Id::new(id) }
            }
            Indexed::Import(import) => {
                if !import.wildcard {
                    self.inner.queue.push_back(BuildEntry {
                        item_meta,
                        build: Build::Import(import),
                        used,
                    });
                }

                meta::Kind::Import(import.entry)
            }
            Indexed::Module => meta::Kind::Module,
        };

        let source = SourceMeta {
            location: item_meta.location,
            path: self
                .sources
                .path(item_meta.location.source_id)
                .map(Into::into),
        };

        Ok(meta::Meta {
            context: false,
            hash: self.pool.item_type_hash(item_meta.item),
            item_meta,
            kind,
            source: Some(source),
            parameters: Hash::EMPTY,
        })
    }

    /// Insert the given name into the unit.
    fn insert_name(&mut self, item: ItemId) {
        let item = self.pool.item(item);
        self.inner.names.insert(item);
    }

    /// Handle an imported indexed entry.
    fn import_indexed(
        &mut self,
        span: Span,
        item_meta: ItemMeta,
        indexed: Indexed,
        used: Used,
    ) -> compile::Result<()> {
        // NB: if we find another indexed entry, queue it up for
        // building and clone its built meta to the other
        // results.
        let entry = indexing::Entry { item_meta, indexed };

        let meta = self.build_indexed_entry(span, entry, used)?;
        self.unit.insert_meta(span, &meta, self.pool)?;
        self.insert_meta(meta).with_span(span)?;
        Ok(())
    }

    /// Remove the indexed entry corresponding to the given item..
    fn remove_indexed(
        &mut self,
        span: Span,
        item: ItemId,
    ) -> compile::Result<Option<indexing::Entry>> {
        // See if there's an index entry we can construct and insert.
        let entries = match self.inner.indexed.remove(&item) {
            Some(entries) => entries,
            None => return Ok(None),
        };

        let mut it = entries.into_iter().peekable();

        let mut cur = match it.next() {
            Some(first) => first,
            None => return Ok(None),
        };

        if it.peek().is_none() {
            return Ok(Some(cur));
        }

        let mut locations = vec![(cur.item_meta.location, cur.item().to_owned())];

        while let Some(oth) = it.next() {
            locations.push((oth.item_meta.location, oth.item().to_owned()));

            if let (Indexed::Import(a), Indexed::Import(b)) = (&cur.indexed, &oth.indexed) {
                if a.wildcard {
                    cur = oth;
                    continue;
                }

                if b.wildcard {
                    continue;
                }
            }

            for oth in it {
                locations.push((oth.item_meta.location, oth.item().to_owned()));
            }

            return Err(compile::Error::new(
                span,
                QueryErrorKind::AmbiguousItem {
                    item: self.pool.item(cur.item_meta.item).to_owned(),
                    locations: locations
                        .into_iter()
                        .map(|(loc, item)| (loc, self.pool.item(item).to_owned()))
                        .collect(),
                },
            ));
        }

        if let Indexed::Import(indexing::Import { wildcard: true, .. }) = &cur.indexed {
            return Err(compile::Error::new(
                span,
                QueryErrorKind::AmbiguousItem {
                    item: self.pool.item(cur.item_meta.item).to_owned(),
                    locations: locations
                        .into_iter()
                        .map(|(loc, item)| (loc, self.pool.item(item).to_owned()))
                        .collect(),
                },
            ));
        }

        Ok(Some(cur))
    }

    /// Walk the names to find the first one that is contained in the unit.
    #[tracing::instrument(skip_all, fields(module = ?self.pool.module_item(module), base = ?self.pool.item(base)))]
    fn convert_initial_path(
        &mut self,
        context: &Context,
        module: ModId,
        base: ItemId,
        local: &ast::Ident,
    ) -> compile::Result<ItemId> {
        let span = local.span;
        let mut base = self.pool.item(base).to_owned();
        debug_assert!(base.starts_with(self.pool.module_item(module)));

        let local = local.resolve(resolve_context!(self))?.to_owned();

        while base.starts_with(self.pool.module_item(module)) {
            base.push(&local);

            tracing::trace!(?base, "testing");

            if self.inner.names.contains(&base) {
                let item = self.pool.alloc_item(&base);

                // TODO: We probably should not engage the whole query meta
                // machinery here.
                if let Some(meta) = self.query_meta(span, item, Used::Used)? {
                    if !matches!(meta.kind, meta::Kind::AssociatedFunction { .. }) {
                        return Ok(self.pool.alloc_item(base));
                    }
                }
            }

            let c = base.pop();
            debug_assert!(c.is_some());

            if base.pop().is_none() {
                break;
            }
        }

        if let Some(item) = self.prelude.get(&local) {
            return Ok(self.pool.alloc_item(item));
        }

        if context.contains_crate(&local) {
            return Ok(self.pool.alloc_item(ItemBuf::with_crate(&local)));
        }

        let new_module = self.pool.module_item(module).extended(&local);
        Ok(self.pool.alloc_item(new_module))
    }

    /// Check that the given item is accessible from the given module.
    fn check_access_to(
        &mut self,
        span: Span,
        from: ModId,
        item: ItemId,
        module: ModId,
        location: Location,
        visibility: Visibility,
        chain: &mut Vec<ImportStep>,
    ) -> compile::Result<()> {
        fn into_chain(chain: Vec<ImportStep>) -> Vec<Location> {
            chain.into_iter().map(|c| c.location).collect()
        }

        let (common, tree) = self
            .pool
            .module_item(from)
            .ancestry(self.pool.module_item(module));
        let mut current_module = common.clone();

        // Check each module from the common ancestrly to the module.
        for c in &tree {
            current_module.push(c);
            let current_module_id = self.pool.alloc_item(&current_module);

            let m = self.pool.module_by_item(current_module_id).ok_or_else(|| {
                compile::Error::new(
                    span,
                    QueryErrorKind::MissingMod {
                        item: current_module.clone(),
                    },
                )
            })?;

            if !m.visibility.is_visible(&common, &current_module) {
                return Err(compile::Error::new(
                    span,
                    QueryErrorKind::NotVisibleMod {
                        chain: into_chain(take(chain)),
                        location: m.location,
                        visibility: m.visibility,
                        item: current_module,
                        from: self.pool.module_item(from).to_owned(),
                    },
                ));
            }
        }

        if !visibility.is_visible_inside(&common, self.pool.module_item(module)) {
            return Err(compile::Error::new(
                span,
                QueryErrorKind::NotVisible {
                    chain: into_chain(take(chain)),
                    location,
                    visibility,
                    item: self.pool.item(item).to_owned(),
                    from: self.pool.module_item(from).to_owned(),
                },
            ));
        }

        Ok(())
    }
}
