/// Backend label used for metrics and operator-facing diagnostics only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendLabel {
    Postgres,
    Turso,
    Redis,
    TursoRouted,
    Sim,
}

impl BackendLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Turso => "turso",
            Self::Redis => "redis",
            Self::TursoRouted => "turso-routed",
            Self::Sim => "sim",
        }
    }
}
