use std::{
    borrow::Borrow,
    collections::HashMap,
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

    #[allow(dead_code)]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_capacity_and_hasher(capacity, Default::default())
    }

    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (&L, &R, &V)> {
        self.left.iter().map(|(l, (r, v))| (l, r, v))
    }

    #[allow(dead_code)]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&L, &R, &mut V)> {
        self.left.iter_mut().map(|(l, (r, v))| (l, r as &R, v))
    }

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
    pub fn contains_left<Q>(&self, left: &Q) -> bool
    where
        L: Borrow<Q>,
        Q: Hash + Eq,
    {
        self.left.contains_key(left)
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
        L: Clone,
        R: Clone,
    {
        if let Some((old_right, old_value)) = self.left.insert(left.clone(), (right.clone(), value))
        {
            self.right.remove(&old_right);
            let old_left = self.right.insert(right, left).unwrap();
            Some((old_left, old_right, old_value))
        } else {
            self.right.insert(right, left);
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

    pub fn with_capacity_and_hasher(capacity: usize, hasher: S) -> Self {
        Self {
            left: HashMap::with_capacity_and_hasher(capacity, hasher.clone()),
            right: HashMap::with_capacity_and_hasher(capacity, hasher),
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
