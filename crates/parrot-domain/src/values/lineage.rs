use std::collections::HashMap;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLineageRecord {
    pub session_id: String,
    pub parent_session_id: String,
    pub name: String,
}

impl AgentLineageRecord {
    pub fn new(
        session_id: impl Into<String>,
        parent_session_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            parent_session_id: parent_session_id.into(),
            name: name.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentDirectoryOccupants {
    pub name_path: Vec<String>,
    pub session_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentDirectoryLineage {
    directories: Vec<AgentDirectoryOccupants>,
    paths: HashMap<String, Vec<String>>,
    directory_indexes: HashMap<String, usize>,
    unresolved: Vec<String>,
}

impl AgentDirectoryLineage {
    pub fn resolve(records: &[AgentLineageRecord]) -> Self {
        let mut lineage = Self::default();
        for record in records {
            let path = if record.parent_session_id.is_empty() {
                Some(vec![record.name.clone()])
            } else {
                lineage
                    .paths
                    .get(&record.parent_session_id)
                    .map(|parent| [parent.as_slice(), &[record.name.clone()]].concat())
            };
            if let Some(path) = path {
                lineage.add(&record.session_id, path);
            } else {
                lineage.unresolved.push(record.session_id.clone());
            }
        }
        lineage
    }

    pub fn directories(&self) -> &[AgentDirectoryOccupants] {
        &self.directories
    }

    pub fn unresolved(&self) -> &[String] {
        &self.unresolved
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.paths.contains_key(session_id)
    }

    pub fn occupants_of(&self, name_path: &[String], self_session_id: &str) -> Vec<String> {
        let mut occupants: Vec<String> = self
            .directory_indexes
            .get(&key(name_path))
            .map(|index| {
                self.directories[*index]
                    .session_ids
                    .iter()
                    .filter(|session_id| session_id.as_str() != self_session_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        occupants.push(self_session_id.to_owned());
        occupants
    }

    fn add(&mut self, session_id: &str, name_path: Vec<String>) {
        self.paths.insert(session_id.to_owned(), name_path.clone());
        let directory_key = key(&name_path);
        let index = *self
            .directory_indexes
            .entry(directory_key)
            .or_insert_with(|| {
                self.directories.push(AgentDirectoryOccupants {
                    name_path,
                    session_ids: Vec::new(),
                });
                self.directories.len() - 1
            });
        self.directories[index]
            .session_ids
            .push(session_id.to_owned());
    }
}

fn key(name_path: &[String]) -> String {
    name_path.join("/")
}
