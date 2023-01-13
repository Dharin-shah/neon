use std::ops::Range;

// There's an alternative non-sync crate im-rc that's supposed to be 20%
// faster. It's not necessary now, but if extra perf is needed that's an option.
use im::OrdMap;

/// Data structure that can efficiently:
/// - find the latest layer by lsn.end at a given key
/// - iterate the latest layers in a key range
/// - insert layers in non-decreasing lsn.start order
///
/// The struct is parameterized over Value for easier
/// testing, but in practice it's some sort of layer.
pub struct LayerCoverage<Value> {
    /// For every change in coverage (as we sweep the key space)
    /// we store (lsn.end, value).
    ///
    /// We use an immutable/persistent tree so that we can keep historic
    /// versions of this coverage without cloning the whole thing and
    /// incurring quadratic memory cost. See HistoricLayerCoverage.
    nodes: OrdMap<i128, Option<(u64, Value)>>,
}

impl<T: Clone> Default for LayerCoverage<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Value: Clone> LayerCoverage<Value> {
    pub fn new() -> Self {
        Self {
            nodes: OrdMap::default(),
        }
    }

    /// Helper function to subdivide the key range without changing any values
    ///
    /// Complexity: O(log N)
    fn add_node(&mut self, key: i128) {
        let value = match self.nodes.range(..=key).last() {
            Some((_, Some(v))) => Some(v.clone()),
            Some((_, None)) => None,
            None => None,
        };
        self.nodes.insert(key, value);
    }

    /// Insert a layer.
    ///
    /// Complexity: worst case O(N), in practice O(log N). See not in implementation.
    pub fn insert(&mut self, key: Range<i128>, lsn: Range<u64>, value: Value) {
        // NOTE The order of the following lines is important!!

        // Add nodes at endpoints
        self.add_node(key.start);
        self.add_node(key.end);

        // Raise the height where necessary
        //
        // NOTE This loop is worst case O(N), but amortized O(log N) in the special
        // case when rectangles have no height. In practice I don't think we'll see
        // the kind of layer intersections needed to trigger O(N) behavior. The worst
        // case is N/2 horizontal layers overlapped with N/2 vertical layers in a
        // grid pattern.
        let mut to_update = Vec::new();
        let mut to_remove = Vec::new();
        let mut prev_covered = false;
        for (k, node) in self.nodes.range(key.clone()) {
            let needs_cover = match node {
                None => true,
                Some((h, _)) => h < &lsn.end,
            };
            if needs_cover {
                match prev_covered {
                    true => to_remove.push(*k),
                    false => to_update.push(*k),
                }
            }
            prev_covered = needs_cover;
        }
        if !prev_covered {
            to_remove.push(key.end);
        }
        for k in to_update {
            self.nodes.insert(k, Some((lsn.end, value.clone())));
        }
        for k in to_remove {
            self.nodes.remove(&k);
        }
    }

    /// Get the latest (by lsn.end) layer at a given key
    ///
    /// Complexity: O(log N)
    pub fn query(&self, key: i128) -> Option<Value> {

        // This is cursed. Commenting out these lines makes the code
        // correct, even though they don't do anything.
        let k1 = self.nodes.get_prev(&key)?.0;
        // let k2 = self.nodes.range(..=key).rev().next()?.0;
        // assert_eq!(k1, k2);

        self.nodes
            // .get_prev(&key)?
            .range(..=key).rev().next()?
            .1
            .as_ref()
            .map(|(_, v)| v.clone())
    }

    /// Iterate the changes in layer coverage in a given range. You will likely
    /// want to start with self.query(key.start), and then follow up with self.range
    ///
    /// Complexity: O(log N + result_size)
    pub fn range(&self, key: Range<i128>) -> impl '_ + Iterator<Item = (i128, Option<Value>)> {
        self.nodes
            .range(key)
            .map(|(k, v)| (*k, v.as_ref().map(|x| x.1.clone())))
    }

    /// O(1) clone
    pub fn clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
        }
    }
}

/// Image and delta coverage at a specific LSN.
pub struct LayerCoverageTuple<Value> {
    pub image_coverage: LayerCoverage<Value>,
    pub delta_coverage: LayerCoverage<Value>,
}

impl<T: Clone> Default for LayerCoverageTuple<T> {
    fn default() -> Self {
        Self {
            image_coverage: LayerCoverage::default(),
            delta_coverage: LayerCoverage::default(),
        }
    }
}

impl<Value: Clone> LayerCoverageTuple<Value> {
    pub fn clone(&self) -> Self {
        Self {
            image_coverage: self.image_coverage.clone(),
            delta_coverage: self.delta_coverage.clone(),
        }
    }
}