use std::{
    borrow::Borrow,
    collections::HashMap,
    fmt::Debug,
    hash::{BuildHasher, Hash, RandomState},
};

#[derive(Debug, Clone)]
pub(crate) struct BiHashMap<L, R, V, S = RandomState> {
    left: HashMap<L, (R, V), S>,
    right: HashMap<R, L, S>,
}

impl<L, R, V> BiHashMap<L, R, V, RandomState> {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "android")))]
    pub fn iter(&self) -> impl Iterator<Item = (&L, &R, &V)> {
        self.left.iter().map(|(l, (r, v))| (l, r, v))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fn clear(&mut self) {
        self.left.clear();
        self.right.clear();
    }
}

impl<L, R, V, S> BiHashMap<L, R, V, S>
where
    L: Eq + Hash,
    R: Eq + Hash,
    S: BuildHasher,
{
    pub fn contains_right<Q>(&self, right: &Q) -> bool
    where
        R: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.right.contains_key(right)
    }

    pub fn get_by_left<Q>(&self, left: &Q) -> Option<(&R, &V)>
    where
        L: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.left.get(left.borrow()).map(|(right, v)| (right, v))
    }

    pub fn get_by_right<Q>(&self, right: &Q) -> Option<(&L, &V)>
    where
        R: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.right.get(right.borrow()).map(|left| {
            let (_, v) = self.left.get(left).unwrap();
            (left, v)
        })
    }

    pub fn insert(&mut self, left: L, right: R, value: V) -> Option<(L, R, V)>
    where
        L: Clone + Debug,
        R: Clone + Debug,
    {
        if let Some((old_right, old_value)) = self.left.insert(left.clone(), (right.clone(), value))
        {
            let old_left = self.right.remove(&old_right).unwrap();
            self.right.insert(right.clone(), left.clone());
            Some((old_left, old_right, old_value))
        } else if let Some(old_left) = self.right.insert(right.clone(), left) {
            let (_, old_value) = self.left.remove(&old_left).unwrap();
            Some((old_left, right, old_value))
        } else {
            None
        }
    }

    pub fn remove_by_left<Q>(&mut self, left: &Q) -> Option<(R, V)>
    where
        L: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some((right, value)) = self.left.remove(left.borrow()) {
            self.right.remove(&right);
            Some((right, value))
        } else {
            None
        }
    }

    pub fn remove_by_right<Q>(&mut self, right: &Q) -> Option<(L, V)>
    where
        R: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(left) = self.right.remove(right.borrow()) {
            let (_, value) = self.left.remove(&left).unwrap();
            Some((left, value))
        } else {
            None
        }
    }
}

impl<L, R, V, S: Clone> BiHashMap<L, R, V, S> {
    pub fn with_hasher(hasher: S) -> Self {
        Self {
            left: HashMap::with_hasher(hasher.clone()),
            right: HashMap::with_hasher(hasher),
        }
    }
}

impl<L, R, V, S: Default + Clone> Default for BiHashMap<L, R, V, S> {
    #[inline]
    fn default() -> Self {
        Self::with_hasher(Default::default())
    }
}

impl<L, R, V, S> IntoIterator for BiHashMap<L, R, V, S> {
    type Item = (L, R, V);
    type IntoIter = std::iter::Map<
        <HashMap<L, (R, V)> as std::iter::IntoIterator>::IntoIter,
        fn((L, (R, V))) -> (L, R, V),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.left.into_iter().map(|(l, (r, v))| (l, r, v))
    }
}

impl<'a, L, R, V, S> IntoIterator for &'a BiHashMap<L, R, V, S> {
    type Item = (&'a L, &'a R, &'a V);
    type IntoIter = std::iter::Map<
        std::collections::hash_map::Iter<'a, L, (R, V)>,
        fn((&'a L, &'a (R, V))) -> (&'a L, &'a R, &'a V),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.left.iter().map(|(l, (r, v))| (l, r, v))
    }
}

#[cfg(test)]
mod tests {
    use crate::bimap::BiHashMap;

    #[test]
    fn test_contains() {
        let mut b = BiHashMap::new();
        b.insert(1, 2, 3);
        assert!(b.contains_right(&2));
        assert!(!b.contains_right(&3));
    }

    #[test]
    fn test_get() {
        let mut b = BiHashMap::new();
        b.insert(1, 2, 3);
        assert_eq!(b.get_by_left(&1), Some((&2, &3)));
        assert_eq!(b.get_by_right(&2), Some((&1, &3)));
        assert_eq!(b.get_by_left(&4), None);
        assert_eq!(b.get_by_right(&5), None);
    }

    #[test]
    fn test_insert() {
        let mut b = BiHashMap::new();
        let result = b.insert(1, 2, 3);
        assert_eq!(result, None);
        assert!(b.contains_right(&2));

        let result = b.insert(1, 4, 5);
        assert_eq!(result, Some((1, 2, 3)));
        assert!(b.contains_right(&4));
        assert!(!b.contains_right(&2));

        let result = b.insert(6, 4, 7);
        assert_eq!(result, Some((1, 4, 5)));
        assert!(b.contains_right(&4));
        assert_eq!(b.get_by_left(&6), Some((&4, &7)));
        assert_eq!(b.get_by_right(&4), Some((&6, &7)));

        let result = b.insert(6, 8, 9);
        assert_eq!(result, Some((6, 4, 7)));
        assert!(b.contains_right(&8));
        assert_eq!(b.get_by_left(&6), Some((&8, &9)));
        assert_eq!(b.get_by_right(&8), Some((&6, &9)));
    }

    #[test]
    fn test_remove() {
        let mut b = BiHashMap::new();
        b.insert(1, 2, 3);
        assert!(b.contains_right(&2));

        let result = b.remove_by_left(&1);
        assert_eq!(result, Some((2, 3)));
        assert!(!b.contains_right(&2));

        b.insert(1, 2, 3);
        assert!(b.contains_right(&2));

        let result = b.remove_by_right(&2);
        assert_eq!(result, Some((1, 3)));
        assert!(!b.contains_right(&2));

        let result = b.remove_by_left(&1);
        assert_eq!(result, None);
        let result = b.remove_by_right(&2);
        assert_eq!(result, None);
    }
}
