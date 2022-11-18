/// Unique peer identifier. The user should assume these can be reused by different peers as
/// peers come and go, i.e. they are not assigned just once for the lifetime of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeerId(usize);

impl PeerId {
    pub fn value(&self) -> usize {
        self.0
    }
}

impl From<mio::Token> for PeerId {
    fn from(token: mio::Token) -> Self {
        Self(token.0)
    }
}

impl From<PeerId> for mio::Token {
    fn from(id: PeerId) -> Self {
        Self(id.0)
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}

impl std::hash::Hash for PeerId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
