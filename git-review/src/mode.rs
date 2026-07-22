#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DiffMode {
    Branch(String),
    MergeBase(String),
    WorkingTree,
}

impl Default for DiffMode {
    fn default() -> Self {
        Self::WorkingTree
    }
}

impl DiffMode {
    pub fn label(&self) -> String {
        match self {
            Self::Branch(b) => format!("vs {b}"),
            Self::MergeBase(b) => format!("uncommitted + {b}"),
            Self::WorkingTree => "uncommitted".to_string(),
        }
    }

    pub fn cycle(&self, parent: Option<&str>) -> DiffMode {
        match self {
            Self::MergeBase(_) => Self::WorkingTree,
            Self::WorkingTree => match parent {
                Some(parent) => Self::MergeBase(parent.to_string()),
                None => Self::WorkingTree,
            },
            other => other.clone(),
        }
    }
}
