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

    pub fn remove_children(&mut self) {
        self.children.clear();
    }

    pub fn descendants(&self) -> impl Iterator<Item = (PathBuf, &T)> {
        PathTrieIter::new(self)
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

pub struct ConsolidatingPathTrie {
    trie: PathTrie<()>,
}

impl ConsolidatingPathTrie {
    pub fn new() -> Self {
        Self {
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
    }

    pub fn values(&self) -> Vec<PathBuf> {
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
        let mut ct = ConsolidatingPathTrie::new();
        ct.insert(PathBuf::from("/a/b"));
        ct.insert(PathBuf::from("/a/c"));
        assert_eq!(
            ct.values(),
            vec![PathBuf::from("/a/b"), PathBuf::from("/a/c")]
        );
    }

    #[test]
    fn consolidate_no_siblings2() {
        let mut ct = ConsolidatingPathTrie::new();
        ct.insert(PathBuf::from("/a/b1"));
        ct.insert(PathBuf::from("/a/b2"));
        assert_eq!(
            ct.values(),
            vec![PathBuf::from("/a/b1"), PathBuf::from("/a/b2")]
        );
    }

    #[test]
    fn consolidate_children() {
        let mut ct = ConsolidatingPathTrie::new();
        ct.insert(PathBuf::from("/a/b"));
        ct.insert(PathBuf::from("/a/b/c"));
        assert_eq!(ct.values(), vec![PathBuf::from("/a/b")]);
    }

    #[test]
    fn consolidate_parent() {
        let mut ct = ConsolidatingPathTrie::new();
        ct.insert(PathBuf::from("/a/b/c"));
        ct.insert(PathBuf::from("/a/b"));
        assert_eq!(ct.values(), vec![PathBuf::from("/a/b")]);
    }
}
