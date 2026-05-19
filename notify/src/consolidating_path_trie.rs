use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct PathTrieNode<T> {
    // sorted for binary search
    children: Vec<(OsString, PathTrieNode<T>)>,
    value: Option<T>,
}

impl<T> Default for PathTrieNode<T> {
    fn default() -> Self {
        Self {
            children: Vec::new(),
            value: None,
        }
    }
}

impl<T> PathTrieNode<T> {
    pub(crate) fn get_child_index(&self, key: &OsStr) -> Result<usize, usize> {
        self.children
            .binary_search_by(|(k, _)| k.as_os_str().cmp(key))
    }

    pub(crate) fn children_len(&self) -> usize {
        self.children.len()
    }

    pub fn remove_children(&mut self) {
        self.children.clear();
    }

    pub fn set_value(&mut self, value: T) {
        self.value = Some(value);
    }

    pub fn descendants(&self) -> impl Iterator<Item = (PathBuf, &T)> {
        PathTrieIter::new(self)
    }

    /// Count the total number of valued nodes in this subtree.
    pub fn count_values(&self) -> usize {
        let mut count = usize::from(self.value.is_some());
        for (_, child) in &self.children {
            count += child.count_values();
        }
        count
    }

    /// Return the depth of the deepest valued node in this subtree.
    pub fn max_value_depth(&self, current_depth: usize) -> usize {
        let mut max = if self.value.is_some() {
            current_depth
        } else {
            0
        };
        for (_, child) in &self.children {
            max = max.max(child.max_value_depth(current_depth + 1));
        }
        max
    }
}

#[derive(Debug)]
pub struct PathTrie<T> {
    root: PathTrieNode<T>,
}

impl<T> PathTrie<T> {
    pub fn new() -> Self {
        Self {
            root: PathTrieNode::default(),
        }
    }

    /// Insert a path with the given value.
    pub fn insert(&mut self, path: impl AsRef<Path>, value: T) -> &mut PathTrieNode<T> {
        let path = path.as_ref();
        let mut current = &mut self.root;
        for component in path.components() {
            let key = component.as_os_str();
            match current.get_child_index(key) {
                Ok(idx) => {
                    current = &mut current.children[idx].1;
                }
                Err(idx) => {
                    let new_node = (key.to_os_string(), PathTrieNode::default());
                    current.children.insert(idx, new_node);
                    current = &mut current.children[idx].1;
                }
            }
        }
        current.value = Some(value);
        current
    }

    /// Get the node associated with the given path.
    /// It may not have a value.
    pub fn get_node_mut(&mut self, path: impl AsRef<Path>) -> Option<&mut PathTrieNode<T>> {
        let path = path.as_ref();
        let mut current = &mut self.root;
        for component in path.components() {
            let key = component.as_os_str();
            let idx = current.get_child_index(key).ok()?;
            current = &mut current.children[idx].1;
        }
        Some(current)
    }

    /// Get the node associated with the given path.
    /// It only returns if the node has a value.
    #[expect(dead_code)]
    pub fn get(&self, path: impl AsRef<Path>) -> Option<&PathTrieNode<T>> {
        let path = path.as_ref();
        let mut current = &self.root;
        for component in path.components() {
            let key = component.as_os_str();
            let idx = current.get_child_index(key).ok()?;
            current = &current.children[idx].1;
        }
        Some(current)
    }

    /// Get the value associated with the nearest ancestor of the given path.
    /// If the path itself has a value, it is returned.
    pub fn get_ancestor(&self, path: impl AsRef<Path>) -> Option<(PathBuf, &PathTrieNode<T>)> {
        let path = path.as_ref();
        let mut current = &self.root;
        for (i, component) in path.components().enumerate() {
            if current.value.is_some() {
                let ancestor_path = path.components().take(i).collect();
                return Some((ancestor_path, current));
            }
            let key = component.as_os_str();
            let idx = current.get_child_index(key).ok()?;
            current = &current.children[idx].1;
        }
        current
            .value
            .as_ref()
            .map(|_| (path.to_path_buf(), current))
    }

    #[cfg(test)]
    pub fn iter(&self) -> PathTrieIter<'_, T> {
        PathTrieIter::new(&self.root)
    }
}

pub struct PathTrieIter<'a, T> {
    // (current node, next index to visit)
    stack: Vec<(&'a PathTrieNode<T>, usize)>,
    // current path
    current_path: PathBuf,
}

impl<'a, T> Iterator for PathTrieIter<'a, T> {
    type Item = (PathBuf, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some((node, child_idx)) = self.stack.last_mut() {
            // check current node
            if *child_idx == 0 {
                *child_idx += 1;
                if let Some(value) = &node.value {
                    return Some((self.current_path.clone(), value));
                }
            }

            // visit children
            let current_child_pos = *child_idx - 1;
            if current_child_pos < node.children.len() {
                let (key, next_node) = &node.children[current_child_pos];
                *child_idx += 1;
                self.current_path.push(key);
                self.stack.push((next_node, 0));
            } else {
                self.stack.pop();
                self.current_path.pop();
            }
        }
        None
    }
}

impl<'a, T> PathTrieIter<'a, T> {
    fn new(root: &'a PathTrieNode<T>) -> Self {
        Self {
            stack: vec![(root, 0)],
            current_path: PathBuf::new(),
        }
    }
}

/// Consolidate all valued descendants of nodes at `target_depth` into
/// the node itself. Does not consolidate nodes at depth < `min_depth`.
fn consolidate_at_depth<V: Default>(
    node: &mut PathTrieNode<V>,
    current_depth: usize,
    target_depth: usize,
    min_depth: usize,
) {
    if current_depth == target_depth && current_depth >= min_depth {
        if node.count_values() > 0 {
            node.set_value(V::default());
            node.remove_children();
        }
        return;
    }
    for (_, child) in &mut node.children {
        consolidate_at_depth(child, current_depth + 1, target_depth, min_depth);
    }
}

pub struct ConsolidatingPathTrie {
    children_consolidation: bool,
    /// Maximum number of paths before consolidation. 0 means disabled.
    max_paths: usize,
    trie: PathTrie<()>,
}

impl ConsolidatingPathTrie {
    const CHILDREN_CONSOLIDATION_THRESHOLD: usize = 10;

    pub fn new(children_consolidation: bool, max_paths: usize) -> Self {
        Self {
            children_consolidation,
            max_paths,
            trie: PathTrie::new(),
        }
    }

    pub fn insert(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        if self.trie.get_ancestor(path).is_some() {
            return;
        }
        let inserted = self.trie.insert(path, ());
        inserted.remove_children();

        if self.children_consolidation {
            for ancestor_path in path.ancestors().skip(1) {
                if let Some(parent_node) = self.trie.get_node_mut(ancestor_path)
                    && parent_node.children_len() >= Self::CHILDREN_CONSOLIDATION_THRESHOLD
                {
                    parent_node.remove_children();
                    parent_node.set_value(());
                } else {
                    break;
                }
            }
        }
    }

    /// Find the depth below which consolidation is safe.
    ///
    /// Walks from the trie root following single-child nodes until hitting
    /// a node with multiple children (a divergence point). If there are
    /// multiple branching levels, the first divergence is protected to
    /// prevent merging distinct subtrees. If there's only one branching
    /// level (single chain to leaf fan-out), consolidation is allowed
    /// at the branching point.
    ///
    /// Example 1: watching `/a/x/y`, `/a/x/z`, `/b/w/v`
    ///   First fork at `/` (depth 1), children `a` and `b` have subtrees.
    ///   Returns 2 (`/a` and `/b` are never merged into `/`).
    ///
    /// Example 2: watching `/root/a/b/file0` .. `/root/a/b/file19`
    ///   First fork at `b` (depth 4), all children are leaves.
    ///   Returns 4 (files can be merged into `/root/a/b`).
    fn min_consolidation_depth(&self) -> usize {
        let mut node = &self.trie.root;
        let mut depth = 0;
        let first_branch_depth;

        // Walk single-child nodes to find first divergence
        loop {
            if node.children.len() <= 1 {
                if let Some((_, child)) = node.children.first() {
                    node = child;
                    depth += 1;
                } else {
                    return 0;
                }
            } else {
                first_branch_depth = depth;
                break;
            }
        }

        // Check if any child of the branching node has further depth
        // (i.e., children that are not leaves). If so, the branches represent
        // distinct subtrees that should be protected from cross-subtree merging.
        let children_have_depth = node
            .children
            .iter()
            .any(|(_, child)| !child.children.is_empty());

        if children_have_depth {
            // Children have subtrees: protect the divergence point
            first_branch_depth + 1
        } else {
            // All children are leaves: single level, allow consolidation
            first_branch_depth
        }
    }

    pub fn values(&mut self) -> Vec<PathBuf> {
        if self.max_paths > 0 {
            let max_paths = self.max_paths;
            let mut count = self.trie.root.count_values();
            if count > max_paths {
                let min_depth = self.min_consolidation_depth();
                let mut depth = self.trie.root.max_value_depth(0);

                // O(D × N) where D = depth range, N = trie nodes. Could be O(N)
                // with a single bottom-up DFS pass, but this only runs once per
                // stream restart which already recreates the FSEventStream.
                while count > max_paths && depth > min_depth {
                    consolidate_at_depth(&mut self.trie.root, 0, depth - 1, min_depth);
                    count = self.trie.root.count_values();
                    depth -= 1;
                }
            }
        }

        self.trie
            .root
            .descendants()
            .map(|(path, ())| path)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let mut t = PathTrie::new();
        t.insert(PathBuf::from("/a"), ());
        t.insert(PathBuf::from("/a/b/c"), ());
        t.insert(PathBuf::from("/a/b/c2"), ());
        assert_eq!(
            t.iter().collect::<Vec<_>>(),
            vec![
                (PathBuf::from("/a"), &()),
                (PathBuf::from("/a/b/c"), &()),
                (PathBuf::from("/a/b/c2"), &()),
            ]
        );
    }

    #[test]
    fn consolidate_no_siblings() {
        for children_consolidation in [true, false] {
            let mut ct = ConsolidatingPathTrie::new(children_consolidation, 0);
            ct.insert(PathBuf::from("/a/b"));
            ct.insert(PathBuf::from("/a/c"));
            assert_eq!(
                ct.values(),
                vec![PathBuf::from("/a/b"), PathBuf::from("/a/c")]
            );
        }
    }

    #[test]
    fn consolidate_no_siblings2() {
        for children_consolidation in [true, false] {
            let mut ct = ConsolidatingPathTrie::new(children_consolidation, 0);
            ct.insert(PathBuf::from("/a/b1"));
            ct.insert(PathBuf::from("/a/b2"));
            assert_eq!(
                ct.values(),
                vec![PathBuf::from("/a/b1"), PathBuf::from("/a/b2")]
            );
        }
    }

    #[test]
    fn consolidate_children() {
        for children_consolidation in [true, false] {
            let mut ct = ConsolidatingPathTrie::new(children_consolidation, 0);
            ct.insert(PathBuf::from("/a/b"));
            ct.insert(PathBuf::from("/a/b/c"));
            assert_eq!(ct.values(), vec![PathBuf::from("/a/b")]);
        }
    }

    #[test]
    fn consolidate_parent() {
        for children_consolidation in [true, false] {
            let mut ct = ConsolidatingPathTrie::new(children_consolidation, 0);
            ct.insert(PathBuf::from("/a/b/c"));
            ct.insert(PathBuf::from("/a/b"));
            assert_eq!(ct.values(), vec![PathBuf::from("/a/b")]);
        }
    }

    #[test]
    fn consolidate_to_single_parent() {
        let mut cr = ConsolidatingPathTrie::new(true, 0);
        for i in 1..=ConsolidatingPathTrie::CHILDREN_CONSOLIDATION_THRESHOLD {
            cr.insert(PathBuf::from(format!("/a/b/c{i}")));
        }
        assert_eq!(cr.values(), vec![PathBuf::from("/a/b")]);

        let mut cr = ConsolidatingPathTrie::new(false, 0);
        for i in 1..=ConsolidatingPathTrie::CHILDREN_CONSOLIDATION_THRESHOLD {
            cr.insert(PathBuf::from(format!("/a/b/c{i}")));
        }
        assert!(cr.values().len() > 1);
    }

    #[test]
    fn consolidate_to_single_parent_nested1() {
        let mut cr = ConsolidatingPathTrie::new(true, 0);
        for i in 1..ConsolidatingPathTrie::CHILDREN_CONSOLIDATION_THRESHOLD {
            cr.insert(PathBuf::from(format!("/a/b/c{i}")));
        }
        for i in 1..=ConsolidatingPathTrie::CHILDREN_CONSOLIDATION_THRESHOLD {
            cr.insert(PathBuf::from(format!("/a/b/cc/d{i}")));
        }
        assert_eq!(cr.values(), vec![PathBuf::from("/a/b")]);
    }

    #[test]
    fn consolidate_to_single_parent_nested2() {
        let mut cr = ConsolidatingPathTrie::new(true, 0);
        cr.insert(PathBuf::from("/a/b/c1"));
        cr.insert(PathBuf::from("/a/b/c2"));
        for i in 1..=ConsolidatingPathTrie::CHILDREN_CONSOLIDATION_THRESHOLD {
            cr.insert(PathBuf::from(format!("/a/b/c3/d{i}")));
        }
        assert_eq!(
            cr.values(),
            vec![
                PathBuf::from("/a/b/c1"),
                PathBuf::from("/a/b/c2"),
                PathBuf::from("/a/b/c3")
            ]
        );
    }

    #[test]
    fn count_values_basic() {
        let mut t = PathTrie::new();
        t.insert(PathBuf::from("/a"), ());
        t.insert(PathBuf::from("/a/b/c"), ());
        t.insert(PathBuf::from("/d"), ());
        assert_eq!(t.root.count_values(), 3);
    }

    #[test]
    fn max_value_depth_basic() {
        let mut t = PathTrie::new();
        t.insert(PathBuf::from("/a"), ());
        t.insert(PathBuf::from("/a/b/c"), ());
        t.insert(PathBuf::from("/d/e"), ());
        assert_eq!(t.root.max_value_depth(0), 4);
    }

    #[test]
    fn max_value_depth_flat() {
        let mut t = PathTrie::new();
        t.insert(PathBuf::from("/a"), ());
        t.insert(PathBuf::from("/b"), ());
        assert_eq!(t.root.max_value_depth(0), 2);
    }

    #[test]
    fn consolidate_at_depth_basic() {
        let mut t = PathTrie::new();
        t.insert(PathBuf::from("/a/b/c1"), ());
        t.insert(PathBuf::from("/a/b/c2"), ());
        t.insert(PathBuf::from("/d/e"), ());

        consolidate_at_depth(&mut t.root, 0, 3, 0);

        let paths: Vec<PathBuf> = t.root.descendants().map(|(p, ())| p).collect();
        assert_eq!(paths, vec![PathBuf::from("/a/b"), PathBuf::from("/d/e")]);
    }

    #[test]
    fn consolidate_at_depth_respects_min_depth() {
        let mut t = PathTrie::new();
        t.insert(PathBuf::from("/a/b"), ());
        t.insert(PathBuf::from("/a/c"), ());

        // Try to consolidate at depth 2 (/a level) but min_depth is 3
        consolidate_at_depth(&mut t.root, 0, 2, 3);

        let paths: Vec<PathBuf> = t.root.descendants().map(|(p, ())| p).collect();
        // Should NOT consolidate because depth 2 < min_depth 3
        assert_eq!(paths, vec![PathBuf::from("/a/b"), PathBuf::from("/a/c")]);
    }

    #[test]
    fn max_paths_no_consolidation_when_under_limit() {
        let mut ct = ConsolidatingPathTrie::new(false, 5);
        ct.insert(PathBuf::from("/a/b"));
        ct.insert(PathBuf::from("/a/c"));
        ct.insert(PathBuf::from("/d/e"));
        assert_eq!(ct.values().len(), 3);
    }

    #[test]
    fn max_paths_consolidates_deepest_first() {
        let mut ct = ConsolidatingPathTrie::new(false, 3);
        ct.insert(PathBuf::from("/a/b/c1"));
        ct.insert(PathBuf::from("/a/b/c2"));
        ct.insert(PathBuf::from("/a/b/c3"));
        ct.insert(PathBuf::from("/d/e"));
        ct.insert(PathBuf::from("/f/g"));

        let values = ct.values();
        assert_eq!(values.len(), 3);
        assert!(values.contains(&PathBuf::from("/a/b")));
        assert!(values.contains(&PathBuf::from("/d/e")));
        assert!(values.contains(&PathBuf::from("/f/g")));
    }

    #[test]
    fn max_paths_respects_subtree_roots() {
        // Two distinct subtrees: /a/... and /b/...
        // Even with aggressive consolidation, /a and /b should remain separate
        let mut ct = ConsolidatingPathTrie::new(false, 2);
        ct.insert(PathBuf::from("/a/x/y1"));
        ct.insert(PathBuf::from("/a/x/y2"));
        ct.insert(PathBuf::from("/b/z/w1"));
        ct.insert(PathBuf::from("/b/z/w2"));

        let values = ct.values();
        // Consolidates within each subtree but never merges /a and /b into /
        assert_eq!(values.len(), 2);
        assert!(values.contains(&PathBuf::from("/a/x")));
        assert!(values.contains(&PathBuf::from("/b/z")));
    }

    #[test]
    fn max_paths_cannot_consolidate_above_divergence() {
        // Paths diverge at / into /a and /b, each with deeper structure
        // max_paths=1 is impossible to satisfy without going above divergence
        let mut ct = ConsolidatingPathTrie::new(false, 1);
        ct.insert(PathBuf::from("/a/x/y"));
        ct.insert(PathBuf::from("/b/z/w"));

        let values = ct.values();
        // Best we can do is /a and /b (2 paths) — can't merge to /
        assert_eq!(values.len(), 2);
        assert!(values.contains(&PathBuf::from("/a")));
        assert!(values.contains(&PathBuf::from("/b")));
    }

    #[test]
    fn max_paths_multi_level_consolidation() {
        let mut ct = ConsolidatingPathTrie::new(false, 2);
        ct.insert(PathBuf::from("/a/b/c/d1"));
        ct.insert(PathBuf::from("/a/b/c/d2"));
        ct.insert(PathBuf::from("/a/b/e/f1"));
        ct.insert(PathBuf::from("/a/b/e/f2"));
        ct.insert(PathBuf::from("/x/y"));

        let values = ct.values();
        assert_eq!(values.len(), 2);
        assert!(values.contains(&PathBuf::from("/a/b")));
        assert!(values.contains(&PathBuf::from("/x/y")));
    }

    #[test]
    fn max_paths_large_scale_multiple_roots() {
        let mut ct = ConsolidatingPathTrie::new(false, 1024);

        for root in ["a", "b", "c", "d"] {
            for i in 0..500 {
                ct.insert(PathBuf::from(format!("/{root}/sub{}/file{i}", i % 10)));
            }
        }

        let values = ct.values();
        assert!(
            values.len() <= 1024,
            "Expected <= 1024 paths, got {}",
            values.len()
        );
        assert!(values.iter().any(|p| p.starts_with("/a")));
        assert!(values.iter().any(|p| p.starts_with("/b")));
        assert!(values.iter().any(|p| p.starts_with("/c")));
        assert!(values.iter().any(|p| p.starts_with("/d")));
    }

    #[test]
    fn max_paths_single_root_deep() {
        let mut ct = ConsolidatingPathTrie::new(false, 5);

        for i in 0..20 {
            ct.insert(PathBuf::from(format!("/root/a/b/file{i}")));
        }

        let values = ct.values();
        assert!(
            values.len() <= 5,
            "Expected <= 5 paths, got {}",
            values.len()
        );
    }

    #[test]
    fn max_paths_none_no_consolidation() {
        let mut ct = ConsolidatingPathTrie::new(false, 0);
        for i in 0..100 {
            ct.insert(PathBuf::from(format!("/root/file{i}")));
        }
        assert_eq!(ct.values().len(), 100);
    }
}
