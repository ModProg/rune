use crate::ast::prelude::*;

#[test]
fn ast_parse() {
    use crate::testing::rt;

    rt::<ast::LitBool>("true");
    rt::<ast::LitBool>("false");
}

/// The boolean literal.
///
/// * `true`.
/// * `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Spanned)]
#[non_exhaustive]
pub struct LitBool {
    /// The span corresponding to the literal.
    pub span: Span,
    /// The value of the literal.
    #[rune(skip)]
    pub value: bool,
}

impl Parse for LitBool {
    fn parse(p: &mut Parser) -> Result<Self> {
        let t = p.next()?;

        let value = match t.kind {
            K![true] => true,
            K![false] => false,
            _ => {
                return Err(compile::Error::expected(t, Expectation::Boolean));
            }
        };

        Ok(Self {
            span: t.span,
            value,
        })
    }
}

impl Peek for LitBool {
    fn peek(p: &mut Peeker<'_>) -> bool {
        matches!(p.nth(0), K![true] | K![false])
    }
}

impl ToTokens for LitBool {
    fn to_tokens(&self, _: &mut MacroContext<'_>, stream: &mut TokenStream) {
        stream.push(ast::Token {
            span: self.span,
            kind: if self.value {
                ast::Kind::True
            } else {
                ast::Kind::False
            },
        });
    }
}
