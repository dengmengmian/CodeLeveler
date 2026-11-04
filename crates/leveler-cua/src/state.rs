//! Observation state: one generation of element references.
//!
//! A reference is a promise about the tree the model just read, not a durable
//! element id. Every tree observation starts a new generation and drops the
//! previous references, so a model cannot keep clicking a button that a later
//! observation replaced. The backend still re-reads the element before acting;
//! a generation match alone never proves the UI is unchanged.

use std::collections::HashMap;

use crate::error::{CuaError, CuaResult};
use crate::types::{Bounds, ElementInfo, Node, WindowInfo};

#[derive(Debug, Clone)]
pub(crate) struct RefRecord {
    pub window: u64,
    pub role: String,
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub path: Vec<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct ObservedWindow {
    pub app: String,
    pub title: String,
    pub bounds: Bounds,
}

#[derive(Debug, Default)]
pub(crate) struct ObservationState {
    generation: u64,
    refs: HashMap<String, RefRecord>,
    observed_windows: HashMap<u64, ObservedWindow>,
}

impl ObservationState {
    pub(crate) fn begin_tree(&mut self) -> u64 {
        self.generation += 1;
        self.refs.clear();
        self.generation
    }

    pub(crate) fn record_windows(&mut self, windows: &[WindowInfo]) {
        for window in windows {
            self.observed_windows.insert(
                window.id,
                ObservedWindow {
                    app: window.app.clone(),
                    title: window.title.clone(),
                    bounds: window.bounds,
                },
            );
        }
    }

    pub(crate) fn observed_window(&self, window: u64) -> Option<&ObservedWindow> {
        self.observed_windows.get(&window)
    }

    pub(crate) fn register_tree(
        &mut self,
        generation: u64,
        window: &WindowInfo,
        root: &Node,
    ) -> ElementInfo {
        self.record_windows(std::slice::from_ref(window));
        let mut next = 0usize;
        self.convert(generation, window, root, &mut next)
    }

    fn convert(
        &mut self,
        generation: u64,
        window: &WindowInfo,
        node: &Node,
        next: &mut usize,
    ) -> ElementInfo {
        let reference = format!("e{generation}-{next}");
        *next += 1;
        self.refs.insert(
            reference.clone(),
            RefRecord {
                window: window.id,
                role: node.role.clone(),
                identifier: node.identifier.clone(),
                title: node.title.clone(),
                path: node.path.clone(),
            },
        );
        ElementInfo {
            reference,
            role: node.role.clone(),
            subrole: node.subrole.clone(),
            title: node.title.clone(),
            description: node.description.clone(),
            value: node.value.clone(),
            identifier: node.identifier.clone(),
            enabled: node.enabled,
            focused: node.focused,
            actions: node.actions.clone(),
            bounds: node.bounds,
            children: node
                .children
                .iter()
                .map(|child| self.convert(generation, window, child, next))
                .collect(),
        }
    }

    pub(crate) fn resolve(&self, reference: &str) -> CuaResult<RefRecord> {
        let generation = reference
            .strip_prefix('e')
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(generation, _)| generation.parse::<u64>().ok())
            .ok_or_else(|| CuaError::UnknownReference(reference.to_string()))?;
        if generation != self.generation {
            return Err(CuaError::StaleReference {
                reference: reference.to_string(),
                reason: format!(
                    "it belongs to observation {generation}, the current observation is {}",
                    self.generation
                ),
            });
        }
        self.refs
            .get(reference)
            .cloned()
            .ok_or_else(|| CuaError::UnknownReference(reference.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Point;

    fn window() -> WindowInfo {
        WindowInfo {
            id: 7,
            pid: 42,
            app: "Fixture".into(),
            title: "Window".into(),
            bounds: Bounds {
                x: 10.0,
                y: 20.0,
                width: 300.0,
                height: 200.0,
            },
            on_screen: true,
            focused: true,
        }
    }

    fn node(role: &str, path: Vec<usize>, children: Vec<Node>) -> Node {
        Node {
            path,
            role: role.into(),
            subrole: None,
            title: Some(role.into()),
            description: None,
            value: None,
            identifier: Some(format!("id-{role}")),
            enabled: Some(true),
            focused: None,
            actions: vec!["AXPress".into()],
            bounds: Some(Bounds {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0,
            }),
            children,
        }
    }

    #[test]
    fn references_are_valid_only_within_their_observation_generation() {
        let mut state = ObservationState::default();
        let window = window();
        let tree = node("AXWindow", vec![], vec![node("AXButton", vec![0], vec![])]);

        let generation = state.begin_tree();
        let rendered = state.register_tree(generation, &window, &tree);
        let button_ref = rendered.children[0].reference.clone();
        assert_eq!(button_ref, format!("e{generation}-1"));

        let record = state.resolve(&button_ref).expect("current reference");
        assert_eq!(record.window, 7);
        assert_eq!(record.role, "AXButton");
        assert_eq!(record.path, vec![0]);
        assert_eq!(record.identifier.as_deref(), Some("id-AXButton"));

        let next = state.begin_tree();
        assert_ne!(next, generation);
        assert!(matches!(
            state.resolve(&button_ref),
            Err(CuaError::StaleReference { .. })
        ));
    }

    #[test]
    fn unknown_and_malformed_references_are_not_resolved() {
        let mut state = ObservationState::default();
        assert!(matches!(
            state.resolve("e0-0"),
            Err(CuaError::UnknownReference(_))
        ));
        state.begin_tree();
        assert!(matches!(
            state.resolve("e0-0"),
            Err(CuaError::StaleReference { .. })
        ));
        assert!(matches!(
            state.resolve("nonsense"),
            Err(CuaError::UnknownReference(_))
        ));
    }

    #[test]
    fn observed_windows_keep_the_bounds_a_coordinate_click_was_aimed_at() {
        let mut state = ObservationState::default();
        state.record_windows(&[window()]);
        let observed = state.observed_window(7).expect("recorded");
        assert_eq!(observed.app, "Fixture");
        assert_eq!(
            observed.bounds.center(),
            Point {
                x: 160.0,
                y: 120.0
            }
        );
    }
}
