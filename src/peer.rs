/// Unique peer identifier. These are unique for the lifetime of the process and strictly
/// incrementing for each new connection. Even if the same peer (in terms of socket address)
/// connects multiple times, a new `PeerId` instance will be issued for each connection.
#[derive(Debug, Clone, Hash, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeerId(pub u64);

impl PeerId {
    pub fn value(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.0))
    }
}
