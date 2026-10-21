//! The single source AgentSessionHierarchy shared by replay and presentation.

use std::collections::{HashMap, HashSet};

use parrot_domain::protocol::messages::parrot::{Event, event::Payload};

#[derive(Clone, Default)]
struct SessionNode {
    name: String,
    parent: String,
}

#[derive(Clone, Default)]
pub struct AgentSessionHierarchy {
    sessions: HashMap<String, SessionNode>,
    root_session_id: Option<String>,
}

impl AgentSessionHierarchy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn root_session_id(&self) -> Option<&str> {
        self.root_session_id.as_deref()
    }

    pub fn reset(&mut self) {
        self.root_session_id = None;
        self.sessions.clear();
    }

    pub fn observe(&mut self, event: &Event) {
        match &event.payload {
            Some(Payload::AgentStarted(agent)) => self.update(
                &event.agent_session_id,
                &agent.parent_agent_session_id,
                &agent.name,
            ),
            Some(Payload::AgentFinished(agent)) => self.update(
                &event.agent_session_id,
                &agent.parent_agent_session_id,
                &agent.name,
            ),
            Some(Payload::AgentFailed(agent)) => self.update(
                &event.agent_session_id,
                &agent.parent_agent_session_id,
                &agent.name,
            ),
            Some(Payload::TurnStarted(_))
                if self.root_session_id.is_none()
                    && !self.is_known_child(&event.agent_session_id) =>
            {
                self.root_session_id = Some(event.agent_session_id.clone());
                self.sessions
                    .entry(event.agent_session_id.clone())
                    .or_default();
            }
            _ => {}
        }
    }

    pub fn observe_root(&mut self, id: &str) {
        if !id.is_empty() {
            self.root_session_id = Some(id.into());
            self.sessions.entry(id.into()).or_default();
        }
    }

    pub fn is_root(&self, id: &str) -> bool {
        self.root_session_id.as_deref() == Some(id)
    }

    pub fn is_child(&self, id: &str) -> bool {
        !self.is_root(id) && (self.root_session_id.is_some() || self.is_known_child(id))
    }

    pub fn get_depth(&self, id: &str) -> usize {
        if id.is_empty() || self.is_root(id) {
            return 0;
        }
        let mut depth = 0;
        let mut current = id;
        let mut seen = HashSet::new();
        while seen.insert(current) {
            let Some(node) = self.sessions.get(current) else {
                break;
            };
            if node.parent.is_empty() {
                return depth.max(1);
            }
            depth += 1;
            if self.is_root(&node.parent) {
                return depth;
            }
            current = &node.parent;
        }
        depth.max(1)
    }

    pub fn get_label<'a>(&'a self, id: &'a str) -> Option<&'a str> {
        if id.is_empty() || self.is_root(id) {
            return None;
        }
        Some(self.sessions.get(id).map_or(
            id,
            |node| {
                if node.name.is_empty() { id } else { &node.name }
            },
        ))
    }

    pub fn resolve_agent_reference(&self, owner: &str, reference: &str) -> String {
        let target = if reference == ".." || reference == "parent" {
            self.sessions
                .get(owner)
                .filter(|node| !node.parent.is_empty())
                .map_or(reference, |node| node.parent.as_str())
        } else {
            reference
        };
        let Some(node) = self.sessions.get(target) else {
            return reference.into();
        };
        if !node.name.is_empty() {
            node.name.clone()
        } else if self.is_root(target) {
            "main".into()
        } else {
            reference.into()
        }
    }

    pub fn observe_owner(&mut self, owner: &str, name: &str, parent: &str, parent_name: &str) {
        if owner.is_empty() {
            return;
        }
        self.update(owner, parent, name);
        if !parent.is_empty() && !parent_name.is_empty() {
            self.update(parent, "", parent_name);
        }
    }

    pub fn is_descendant(&self, id: &str, ancestor: &str) -> bool {
        let mut current = id;
        for _ in 0..self.sessions.len() {
            let Some(node) = self.sessions.get(current) else {
                break;
            };
            if node.parent == ancestor {
                return true;
            }
            if node.parent.is_empty() {
                return false;
            }
            current = &node.parent;
        }
        false
    }

    pub fn get_post_order<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, usize> {
        let requested: HashSet<String> = ids.into_iter().map(str::to_owned).collect();
        let mut relevant = requested.clone();
        for id in &requested {
            let mut current = id.as_str();
            let mut seen = HashSet::new();
            while seen.insert(current) {
                let Some(node) = self.sessions.get(current) else {
                    break;
                };
                if node.parent.is_empty() {
                    break;
                }
                current = &node.parent;
                relevant.insert(current.into());
            }
        }
        let mut children: HashMap<&str, Vec<&str>> = relevant
            .iter()
            .map(|id| (id.as_str(), Vec::new()))
            .collect();
        for id in &relevant {
            if let Some(node) = self.sessions.get(id) {
                if let Some(siblings) = children.get_mut(node.parent.as_str()) {
                    siblings.push(id);
                }
            }
        }
        for siblings in children.values_mut() {
            siblings.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        }
        let mut roots: Vec<_> = relevant
            .iter()
            .map(String::as_str)
            .filter(|id| {
                self.sessions
                    .get(*id)
                    .is_none_or(|node| node.parent.is_empty() || !relevant.contains(&node.parent))
            })
            .collect();
        roots.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        if let Some(root) = self.root_session_id.as_deref() {
            if let Some(index) = roots.iter().position(|id| *id == root) {
                roots.remove(index);
                roots.push(root);
            }
        }
        let mut ordered = Vec::new();
        let mut seen = HashSet::new();
        fn append<'a>(
            id: &'a str,
            children: &HashMap<&'a str, Vec<&'a str>>,
            requested: &HashSet<String>,
            seen: &mut HashSet<&'a str>,
            ordered: &mut Vec<String>,
        ) {
            if !seen.insert(id) {
                return;
            }
            for child in &children[id] {
                append(child, children, requested, seen, ordered);
            }
            if requested.contains(id) {
                ordered.push(id.into());
            }
        }
        for id in roots {
            append(id, &children, &requested, &mut seen, &mut ordered);
        }
        let mut remaining: Vec<_> = relevant.iter().map(String::as_str).collect();
        remaining.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        for id in remaining {
            append(id, &children, &requested, &mut seen, &mut ordered);
        }
        ordered
            .into_iter()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect()
    }

    fn is_known_child(&self, id: &str) -> bool {
        self.sessions
            .get(id)
            .is_some_and(|node| !node.parent.is_empty())
    }

    fn update(&mut self, id: &str, parent: &str, name: &str) {
        let node = self.sessions.entry(id.into()).or_default();
        if !name.is_empty() {
            node.name = name.into();
        }
        if !parent.is_empty() {
            node.parent = parent.into();
            self.sessions.entry(parent.into()).or_default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_child_depth_labels_and_parent_references_share_one_hierarchy() {
        let mut hierarchy = AgentSessionHierarchy::new();
        hierarchy.observe_root("root");
        hierarchy.observe_owner("worker", "worker-name", "root", "");
        hierarchy.observe_owner("nested", "", "worker", "");
        assert!(hierarchy.is_root("root"));
        assert!(hierarchy.is_child("worker"));
        assert_eq!(hierarchy.get_depth("root"), 0);
        assert_eq!(hierarchy.get_depth("worker"), 1);
        assert_eq!(hierarchy.get_depth("nested"), 2);
        assert_eq!(hierarchy.get_label("root"), None);
        assert_eq!(hierarchy.get_label("worker"), Some("worker-name"));
        assert_eq!(hierarchy.get_label("unknown"), Some("unknown"));
        assert_eq!(hierarchy.resolve_agent_reference("worker", ".."), "main");
        assert_eq!(
            hierarchy.resolve_agent_reference("nested", "parent"),
            "worker-name"
        );
        assert!(hierarchy.is_descendant("nested", "root"));
        assert!(!hierarchy.is_descendant("root", "nested"));
        hierarchy.reset();
        assert_eq!(hierarchy.root_session_id(), None);
        assert!(!hierarchy.is_child("worker"));
    }

    #[test]
    fn post_order_places_descendants_first_and_uses_utf16_ordinal_siblings() {
        let mut hierarchy = AgentSessionHierarchy::new();
        hierarchy.observe_root("root");
        hierarchy.observe_owner("\u{10000}", "", "root", "");
        hierarchy.observe_owner("\u{e000}", "", "root", "");
        hierarchy.observe_owner("nested", "", "\u{10000}", "");
        let order = hierarchy.get_post_order(["root", "\u{e000}", "\u{10000}", "nested"]);
        assert_eq!(order["nested"], 0);
        assert_eq!(order["\u{10000}"], 1);
        assert_eq!(order["\u{e000}"], 2);
        assert_eq!(order["root"], 3);
    }

    #[test]
    fn cyclic_relationships_terminate_and_keep_requested_nodes_once() {
        let mut hierarchy = AgentSessionHierarchy::new();
        hierarchy.observe_owner("a", "", "b", "");
        hierarchy.observe_owner("b", "", "a", "");
        assert_eq!(hierarchy.get_depth("a"), 2);
        assert!(!hierarchy.is_descendant("a", "missing"));
        let order = hierarchy.get_post_order(["a", "b", "a"]);
        assert_eq!(order.len(), 2);
        assert_eq!(order["b"], 0);
        assert_eq!(order["a"], 1);
    }
}
