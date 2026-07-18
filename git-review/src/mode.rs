#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffMode {
    WorkingTree,
    Staged,
    Branch(String),
    MergeBase(String),
}

impl Default for DiffMode {
    fn default() -> Self {
        Self::WorkingTree
    }
}

impl DiffMode {
    pub fn label(&self) -> String {
        match self {
            Self::WorkingTree => "working tree".to_string(),
            Self::Staged => "staged".to_string(),
            Self::Branch(b) => format!("vs {b}"),
            Self::MergeBase(b) => format!("merge-base {b}"),
        }
    }

    pub fn cycle(&self) -> DiffMode {
        match self {
            Self::WorkingTree => Self::Staged,
            Self::Staged => Self::WorkingTree,
            other => other.clone(),
        }
    }
}
