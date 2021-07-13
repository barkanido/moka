use super::{cache::Cache, ConcurrentCacheExt};
use crate::PredicateError;

use std::{
    borrow::Borrow,
    collections::hash_map::RandomState,
    error::Error,
    hash::{BuildHasher, Hash, Hasher},
    sync::Arc,
    time::Duration,
};

/// A thread-safe concurrent in-memory cache, with multiple internal segments.
///
/// `SegmentedCache` has multiple internal [`Cache`][cache-struct] instances for
/// increased concurrent update performance. However, it has little overheads on
/// retrievals and updates for managing these segments.
///
/// For usage examples, see the document of the [`Cache`][cache-struct].
///
/// [cache-struct]: ./struct.Cache.html
///
pub struct SegmentedCache<K, V, S = RandomState> {
    inner: Arc<Inner<K, V, S>>,
}

unsafe impl<K, V, S> Send for SegmentedCache<K, V, S>
where
    K: Send + Sync,
    V: Send + Sync,
    S: Send,
{
}

unsafe impl<K, V, S> Sync for SegmentedCache<K, V, S>
where
    K: Send + Sync,
    V: Send + Sync,
    S: Sync,
{
}

impl<K, V, S> Clone for SegmentedCache<K, V, S> {
    /// Makes a clone of this shared cache.
    ///
    /// This operation is cheap as it only creates thread-safe reference counted
    /// pointers to the shared internal data structures.
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K, V> SegmentedCache<K, V, RandomState>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Constructs a new `SegmentedCache<K, V>` that has multiple internal
    /// segments and will store up to the `max_capacity` entries.
    ///
    /// To adjust various configuration knobs such as `initial_capacity` or
    /// `time_to_live`, use the [`CacheBuilder`][builder-struct].
    ///
    /// [builder-struct]: ./struct.CacheBuilder.html
    ///
    /// # Panics
    ///
    /// Panics if `num_segments` is 0.
    pub fn new(max_capacity: usize, num_segments: usize) -> Self {
        let build_hasher = RandomState::default();
        Self::with_everything(
            max_capacity,
            None,
            num_segments,
            build_hasher,
            None,
            None,
            false,
        )
    }
}

impl<K, V, S> SegmentedCache<K, V, S>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync + 'static,
{
    /// # Panics
    ///
    /// Panics if `num_segments` is 0.
    pub(crate) fn with_everything(
        max_capacity: usize,
        initial_capacity: Option<usize>,
        num_segments: usize,
        build_hasher: S,
        time_to_live: Option<Duration>,
        time_to_idle: Option<Duration>,
        invalidator_enabled: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner::new(
                max_capacity,
                initial_capacity,
                num_segments,
                build_hasher,
                time_to_live,
                time_to_idle,
                invalidator_enabled,
            )),
        }
    }

    /// Returns a _clone_ of the value corresponding to the key.
    ///
    /// If you want to store values that will be expensive to clone, wrap them by
    /// `std::sync::Arc` before storing in a cache. [`Arc`][rustdoc-std-arc] is a
    /// thread-safe reference-counted pointer and its `clone()` method is cheap.
    ///
    /// The key may be any borrowed form of the cache's key type, but `Hash` and `Eq`
    /// on the borrowed form _must_ match those for the key type.
    ///
    /// [rustdoc-std-arc]: https://doc.rust-lang.org/stable/std/sync/struct.Arc.html
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        Arc<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.inner.hash(key);
        self.inner.select(hash).get_with_hash(key, hash)
    }

    /// Ensures the value of the key exists by inserting the result of the init
    /// function if not exist, and returns a _clone_ of the value.
    ///
    /// This method prevents to evaluate the init function multiple times on the same
    /// key even if the method is concurrently called by many threads; only one of
    /// the calls evaluates its function, and other calls wait for that function to
    /// complete.
    pub fn get_or_insert_with(&self, key: K, init: impl FnOnce() -> V) -> V {
        let hash = self.inner.hash(&key);
        let key = Arc::new(key);
        self.inner
            .select(hash)
            .get_or_insert_with_hash_and_fun(key, hash, init)
    }

    /// Try to ensure the value of the key exists by inserting an `Ok` result of the
    /// init function if not exist, and returns a _clone_ of the value or the `Err`
    /// returned by the function.
    ///
    /// This method prevents to evaluate the init function multiple times on the same
    /// key even if the method is concurrently called by many threads; only one of
    /// the calls evaluates its function, and other calls wait for that function to
    /// complete.
    pub fn get_or_try_insert_with<F>(
        &self,
        key: K,
        init: F,
    ) -> Result<V, Arc<Box<dyn Error + Send + Sync + 'static>>>
    where
        F: FnOnce() -> Result<V, Box<dyn Error + Send + Sync + 'static>>,
    {
        let hash = self.inner.hash(&key);
        let key = Arc::new(key);
        self.inner
            .select(hash)
            .get_or_try_insert_with_hash_and_fun(key, hash, init)
    }

    /// Inserts a key-value pair into the cache.
    ///
    /// If the cache has this key present, the value is updated.
    pub fn insert(&self, key: K, value: V) {
        let hash = self.inner.hash(&key);
        let key = Arc::new(key);
        self.inner.select(hash).insert_with_hash(key, hash, value);
    }

    /// Discards any cached value for the key.
    ///
    /// The key may be any borrowed form of the cache's key type, but `Hash` and `Eq`
    /// on the borrowed form _must_ match those for the key type.
    pub fn invalidate<Q>(&self, key: &Q)
    where
        Arc<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.inner.hash(key);
        self.inner.select(hash).invalidate_with_hash(key, hash);
    }

    /// Discards all cached values.
    ///
    /// This method returns immediately and a background thread will evict all the
    /// cached values inserted before the time when this method was called. It is
    /// guaranteed that the `get` method must not return these invalidated values
    /// even if they have not been evicted.
    ///
    /// Like the `invalidate` method, this method does not clear the historic
    /// popularity estimator of keys so that it retains the client activities of
    /// trying to retrieve an item.
    pub fn invalidate_all(&self) {
        for segment in self.inner.segments.iter() {
            segment.invalidate_all();
        }
    }

    /// Discards cached values that satisfy a predicate.
    ///
    /// `invalidate_entries_if` takes a closure that returns `true` or `false`. This
    /// method returns immediately and a background thread will apply the closure to
    /// each cached value inserted before the time when `invalidate_entries_if` was
    /// called. If the closure returns `true` on a value, that value will be evicted
    /// from the cache.
    ///
    /// Also the `get` method will apply the closure to a value to determine if it
    /// should have been invalidated. Therefore, it is guaranteed that the `get`
    /// method must not return invalidated values.
    ///
    /// Note that you must call
    /// [`CacheBuilder::support_invalidation_closures`][support-invalidation-closures]
    /// at the cache creation time as the cache needs to maintain additional internal
    /// data structures to support this method. Otherwise, calling this method will
    /// fail with a
    /// [`PredicateError::InvalidationClosuresDisabled`][invalidation-disabled-error].
    ///
    /// Like the `invalidate` method, this method does not clear the historic
    /// popularity estimator of keys so that it retains the client activities of
    /// trying to retrieve an item.
    ///
    /// [support-invalidation-closures]: ./struct.CacheBuilder.html#method.support_invalidation_closures
    /// [invalidation-disabled-error]: ../enum.PredicateError.html#variant.InvalidationClosuresDisabled
    pub fn invalidate_entries_if<F>(&self, predicate: F) -> Result<(), PredicateError>
    where
        F: Fn(&K, &V) -> bool + Send + Sync + 'static,
    {
        let pred = Arc::new(predicate);
        for segment in self.inner.segments.iter() {
            segment.invalidate_entries_with_arc_fun(Arc::clone(&pred))?;
        }
        Ok(())
    }

    /// Returns the `max_capacity` of this cache.
    pub fn max_capacity(&self) -> usize {
        self.inner.desired_capacity
    }

    /// Returns the `time_to_live` of this cache.
    pub fn time_to_live(&self) -> Option<Duration> {
        self.inner.segments[0].time_to_live()
    }

    /// Returns the `time_to_idle` of this cache.
    pub fn time_to_idle(&self) -> Option<Duration> {
        self.inner.segments[0].time_to_idle()
    }

    /// Returns the number of internal segments of this cache.
    pub fn num_segments(&self) -> usize {
        self.inner.segments.len()
    }

    // /// This is used by unit tests to get consistent result.
    // #[cfg(test)]
    // pub(crate) fn reconfigure_for_testing(&mut self) {
    //     // Stop the housekeeping job that may cause sync() method to return earlier.
    //     for segment in self.inner.segments.iter_mut() {
    //         segment.reconfigure_for_testing()
    //     }
    // }
}

impl<K, V, S> ConcurrentCacheExt<K, V> for SegmentedCache<K, V, S>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync + 'static,
{
    fn sync(&self) {
        for segment in self.inner.segments.iter() {
            segment.sync();
        }
    }
}

// For unit tests.
#[cfg(test)]
impl<K, V, S> SegmentedCache<K, V, S>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync + 'static,
{
    fn table_size(&self) -> usize {
        self.inner.segments.iter().map(|seg| seg.table_size()).sum()
    }

    fn invalidation_predicate_count(&self) -> usize {
        self.inner
            .segments
            .iter()
            .map(|seg| seg.invalidation_predicate_count())
            .sum()
    }

    fn reconfigure_for_testing(&mut self) {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("There are other strong reference to self.inner Arc");

        for segment in inner.segments.iter_mut() {
            segment.reconfigure_for_testing();
        }
    }

    fn create_mock_expiration_clock(&self) -> MockExpirationClock {
        let mut exp_clock = MockExpirationClock::default();

        for segment in self.inner.segments.iter() {
            let (clock, mock) = quanta::Clock::mock();
            segment.set_expiration_clock(Some(clock));
            exp_clock.mocks.push(mock);
        }

        exp_clock
    }
}

// For unit tests.
#[cfg(test)]
#[derive(Default)]
struct MockExpirationClock {
    mocks: Vec<Arc<quanta::Mock>>,
}

#[cfg(test)]
impl MockExpirationClock {
    fn increment(&mut self, duration: Duration) {
        for mock in &mut self.mocks {
            mock.increment(duration);
        }
    }
}

struct Inner<K, V, S> {
    desired_capacity: usize,
    segments: Box<[Cache<K, V, S>]>,
    build_hasher: S,
    segment_shift: u32,
}

impl<K, V, S> Inner<K, V, S>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    S: BuildHasher + Clone + Send + Sync + 'static,
{
    /// # Panics
    ///
    /// Panics if `num_segments` is 0.
    fn new(
        max_capacity: usize,
        initial_capacity: Option<usize>,
        num_segments: usize,
        build_hasher: S,
        time_to_live: Option<Duration>,
        time_to_idle: Option<Duration>,
        invalidator_enabled: bool,
    ) -> Self {
        assert!(num_segments > 0);

        let actual_num_segments = num_segments.next_power_of_two();
        let segment_shift = 64 - actual_num_segments.trailing_zeros();
        // TODO: Round up.
        let seg_capacity = max_capacity / actual_num_segments;
        let seg_init_capacity = initial_capacity.map(|cap| cap / actual_num_segments);
        // NOTE: We cannot initialize the segments as `vec![cache; actual_num_segments]`
        // because Cache::clone() does not clone its inner but shares the same inner.
        let segments = (0..num_segments)
            .map(|_| {
                Cache::with_everything(
                    seg_capacity,
                    seg_init_capacity,
                    build_hasher.clone(),
                    time_to_live,
                    time_to_idle,
                    invalidator_enabled,
                )
            })
            .collect::<Vec<_>>();

        Self {
            desired_capacity: max_capacity,
            segments: segments.into_boxed_slice(),
            build_hasher,
            segment_shift,
        }
    }

    #[inline]
    fn hash<Q>(&self, key: &Q) -> u64
    where
        Arc<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut hasher = self.build_hasher.build_hasher();
        key.hash(&mut hasher);
        hasher.finish()
    }

    #[inline]
    fn select(&self, hash: u64) -> &Cache<K, V, S> {
        let index = self.segment_index_from_hash(hash);
        &self.segments[index]
    }

    #[inline]
    fn segment_index_from_hash(&self, hash: u64) -> usize {
        if self.segment_shift == 64 {
            0
        } else {
            (hash >> self.segment_shift) as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConcurrentCacheExt, SegmentedCache};
    use crate::sync::CacheBuilder;
    use std::time::Duration;

    #[test]
    fn basic_single_thread() {
        let mut cache = SegmentedCache::new(3, 1);
        cache.reconfigure_for_testing();

        // Make the cache exterior immutable.
        let cache = cache;

        cache.insert("a", "alice");
        cache.insert("b", "bob");
        assert_eq!(cache.get(&"a"), Some("alice"));
        assert_eq!(cache.get(&"b"), Some("bob"));
        cache.sync();
        // counts: a -> 1, b -> 1

        cache.insert("c", "cindy");
        assert_eq!(cache.get(&"c"), Some("cindy"));
        // counts: a -> 1, b -> 1, c -> 1
        cache.sync();

        assert_eq!(cache.get(&"a"), Some("alice"));
        assert_eq!(cache.get(&"b"), Some("bob"));
        cache.sync();
        // counts: a -> 2, b -> 2, c -> 1

        // "d" should not be admitted because its frequency is too low.
        cache.insert("d", "david"); //   count: d -> 0
        cache.sync();
        assert_eq!(cache.get(&"d"), None); //   d -> 1

        cache.insert("d", "david");
        cache.sync();
        assert_eq!(cache.get(&"d"), None); //   d -> 2

        // "d" should be admitted and "c" should be evicted
        // because d's frequency is higher then c's.
        cache.insert("d", "dennis");
        cache.sync();
        assert_eq!(cache.get(&"a"), Some("alice"));
        assert_eq!(cache.get(&"b"), Some("bob"));
        assert_eq!(cache.get(&"c"), None);
        assert_eq!(cache.get(&"d"), Some("dennis"));

        cache.invalidate(&"b");
    }

    #[test]
    fn basic_multi_threads() {
        let num_threads = 4;

        let mut cache = SegmentedCache::new(100, num_threads);
        cache.reconfigure_for_testing();

        // Make the cache exterior immutable.
        let cache = cache;

        let handles = (0..num_threads)
            .map(|id| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    cache.insert(10, format!("{}-100", id));
                    cache.get(&10);
                    cache.sync();
                    cache.insert(20, format!("{}-200", id));
                    cache.invalidate(&10);
                })
            })
            .collect::<Vec<_>>();

        handles.into_iter().for_each(|h| h.join().expect("Failed"));

        cache.sync();

        assert!(cache.get(&10).is_none());
        assert!(cache.get(&20).is_some());
    }

    #[test]
    fn invalidate_all() {
        let mut cache = SegmentedCache::new(100, 4);
        cache.reconfigure_for_testing();

        // Make the cache exterior immutable.
        let cache = cache;

        cache.insert("a", "alice");
        cache.insert("b", "bob");
        cache.insert("c", "cindy");
        assert_eq!(cache.get(&"a"), Some("alice"));
        assert_eq!(cache.get(&"b"), Some("bob"));
        assert_eq!(cache.get(&"c"), Some("cindy"));
        cache.sync();

        cache.invalidate_all();
        cache.sync();

        cache.insert("d", "david");
        cache.sync();

        assert!(cache.get(&"a").is_none());
        assert!(cache.get(&"b").is_none());
        assert!(cache.get(&"c").is_none());
        assert_eq!(cache.get(&"d"), Some("david"));
    }

    #[test]
    fn invalidate_entries_if() -> Result<(), Box<dyn std::error::Error>> {
        use std::collections::HashSet;

        const SEGMENTS: usize = 4;

        let mut cache = CacheBuilder::new(100)
            .segments(SEGMENTS)
            .support_invalidation_closures()
            .build();
        cache.reconfigure_for_testing();

        let mut mock = cache.create_mock_expiration_clock();

        // Make the cache exterior immutable.
        let cache = cache;

        cache.insert(0, "alice");
        cache.insert(1, "bob");
        cache.insert(2, "alex");
        cache.sync();
        mock.increment(Duration::from_secs(5)); // 5 secs from the start.
        cache.sync();

        assert_eq!(cache.get(&0), Some("alice"));
        assert_eq!(cache.get(&1), Some("bob"));
        assert_eq!(cache.get(&2), Some("alex"));

        let names = ["alice", "alex"].iter().cloned().collect::<HashSet<_>>();
        cache.invalidate_entries_if(move |_k, &v| names.contains(v))?;
        assert_eq!(cache.invalidation_predicate_count(), SEGMENTS * 1);

        mock.increment(Duration::from_secs(5)); // 10 secs from the start.

        cache.insert(3, "alice");

        // Run the invalidation task and wait for it to finish. (TODO: Need a better way than sleeping)
        cache.sync(); // To submit the invalidation task.
        std::thread::sleep(Duration::from_millis(200));
        cache.sync(); // To process the task result.
        std::thread::sleep(Duration::from_millis(200));

        assert!(cache.get(&0).is_none());
        assert!(cache.get(&2).is_none());
        assert_eq!(cache.get(&1), Some("bob"));
        // This should survive as it was inserted after calling invalidate_entries_if.
        assert_eq!(cache.get(&3), Some("alice"));
        assert_eq!(cache.table_size(), 2);
        assert_eq!(cache.invalidation_predicate_count(), SEGMENTS * 0);

        mock.increment(Duration::from_secs(5)); // 15 secs from the start.

        cache.invalidate_entries_if(|_k, &v| v == "alice")?;
        cache.invalidate_entries_if(|_k, &v| v == "bob")?;
        assert_eq!(cache.invalidation_predicate_count(), SEGMENTS * 2);

        // Run the invalidation task and wait for it to finish. (TODO: Need a better way than sleeping)
        cache.sync(); // To submit the invalidation task.
        std::thread::sleep(Duration::from_millis(200));
        cache.sync(); // To process the task result.
        std::thread::sleep(Duration::from_millis(200));

        assert!(cache.get(&1).is_none());
        assert!(cache.get(&3).is_none());
        assert_eq!(cache.table_size(), 0);
        assert_eq!(cache.invalidation_predicate_count(), SEGMENTS * 0);

        Ok(())
    }

    #[test]
    fn get_or_insert_with() {
        use std::thread::{sleep, spawn};

        let cache = SegmentedCache::new(100, 4);
        const KEY: u32 = 0;

        // This test will run five threads:
        //
        // Thread1 will be the first thread to call `get_or_insert_with` for a key, so
        // its async block will be evaluated and then a &str value "thread1" will be
        // inserted to the cache.
        let thread1 = {
            let cache1 = cache.clone();
            spawn(move || {
                // Call `get_or_insert_with` immediately.
                let v = cache1.get_or_insert_with(KEY, || {
                    // Wait for 300 ms and return a &str value.
                    sleep(Duration::from_millis(300));
                    "thread1"
                });
                assert_eq!(v, "thread1");
            })
        };

        // Thread2 will be the second thread to call `get_or_insert_with` for the same
        // key, so its async block will not be evaluated. Once thread1's async block
        // finishes, it will get the value inserted by thread1's async block.
        let thread2 = {
            let cache2 = cache.clone();
            spawn(move || {
                // Wait for 100 ms before calling `get_or_insert_with`.
                sleep(Duration::from_millis(100));
                let v = cache2.get_or_insert_with(KEY, || unreachable!());
                assert_eq!(v, "thread1");
            })
        };

        // Thread3 will be the third thread to call `get_or_insert_with` for the same
        // key. By the time it calls, thread1's async block should have finished
        // already and the value should be already inserted to the cache. So its
        // async block will not be evaluated and will get the value insert by thread1's
        // async block immediately.
        let thread3 = {
            let cache3 = cache.clone();
            spawn(move || {
                // Wait for 400 ms before calling `get_or_insert_with`.
                sleep(Duration::from_millis(400));
                let v = cache3.get_or_insert_with(KEY, || unreachable!());
                assert_eq!(v, "thread1");
            })
        };

        // Thread4 will call `get` for the same key. It will call when thread1's async
        // block is still running, so it will get none for the key.
        let thread4 = {
            let cache4 = cache.clone();
            spawn(move || {
                // Wait for 200 ms before calling `get`.
                sleep(Duration::from_millis(200));
                let maybe_v = cache4.get(&KEY);
                assert!(maybe_v.is_none());
            })
        };

        // Thread5 will call `get` for the same key. It will call after thread1's async
        // block finished, so it will get the value insert by thread1's async block.
        let thread5 = {
            let cache5 = cache.clone();
            spawn(move || {
                // Wait for 400 ms before calling `get`.
                sleep(Duration::from_millis(400));
                let maybe_v = cache5.get(&KEY);
                assert_eq!(maybe_v, Some("thread1"));
            })
        };

        for t in vec![thread1, thread2, thread3, thread4, thread5] {
            t.join().expect("Failed to join");
        }
    }

    #[test]
    fn get_or_try_insert_with() {
        use std::thread::{sleep, spawn};

        let cache = SegmentedCache::new(100, 4);
        const KEY: u32 = 0;

        // This test will run eight async threads:
        //
        // Thread1 will be the first thread to call `get_or_insert_with` for a key, so
        // its async block will be evaluated and then an error will be returned.
        // Nothing will be inserted to the cache.
        let thread1 = {
            let cache1 = cache.clone();
            spawn(move || {
                // Call `get_or_try_insert_with` immediately.
                let v = cache1.get_or_try_insert_with(KEY, || {
                    // Wait for 300 ms and return an error.
                    sleep(Duration::from_millis(300));
                    Err("thread1 error".into())
                });
                assert!(v.is_err());
            })
        };

        // Thread2 will be the second thread to call `get_or_insert_with` for the same
        // key, so its async block will not be evaluated. Once thread1's async block
        // finishes, it will get the same error value returned by thread1's async
        // block.
        let thread2 = {
            let cache2 = cache.clone();
            spawn(move || {
                // Wait for 100 ms before calling `get_or_try_insert_with`.
                sleep(Duration::from_millis(100));
                let v = cache2.get_or_try_insert_with(KEY, || unreachable!());
                assert!(v.is_err());
            })
        };

        // Thread3 will be the third thread to call `get_or_insert_with` for the same
        // key. By the time it calls, thread1's async block should have finished
        // already, but the key still does not exist in the cache. So its async block
        // will be evaluated and then an okay &str value will be returned. That value
        // will be inserted to the cache.
        let thread3 = {
            let cache3 = cache.clone();
            spawn(move || {
                // Wait for 400 ms before calling `get_or_try_insert_with`.
                sleep(Duration::from_millis(400));
                let v = cache3.get_or_try_insert_with(KEY, || {
                    // Wait for 300 ms and return an Ok(&str) value.
                    sleep(Duration::from_millis(300));
                    Ok("thread3")
                });
                assert_eq!(v.unwrap(), "thread3");
            })
        };

        // thread4 will be the fourth thread to call `get_or_insert_with` for the same
        // key. So its async block will not be evaluated. Once thread3's async block
        // finishes, it will get the same okay &str value.
        let thread4 = {
            let cache4 = cache.clone();
            spawn(move || {
                // Wait for 500 ms before calling `get_or_try_insert_with`.
                sleep(Duration::from_millis(500));
                let v = cache4.get_or_try_insert_with(KEY, || unreachable!());
                assert_eq!(v.unwrap(), "thread3");
            })
        };

        // Thread5 will be the fifth thread to call `get_or_insert_with` for the same
        // key. So its async block will not be evaluated. By the time it calls,
        // thread3's async block should have finished already, so its async block will
        // not be evaluated and will get the value insert by thread3's async block
        // immediately.
        let thread5 = {
            let cache5 = cache.clone();
            spawn(move || {
                // Wait for 800 ms before calling `get_or_try_insert_with`.
                sleep(Duration::from_millis(800));
                let v = cache5.get_or_try_insert_with(KEY, || unreachable!());
                assert_eq!(v.unwrap(), "thread3");
            })
        };

        // Thread6 will call `get` for the same key. It will call when thread1's async
        // block is still running, so it will get none for the key.
        let thread6 = {
            let cache6 = cache.clone();
            spawn(move || {
                // Wait for 200 ms before calling `get`.
                sleep(Duration::from_millis(200));
                let maybe_v = cache6.get(&KEY);
                assert!(maybe_v.is_none());
            })
        };

        // Thread7 will call `get` for the same key. It will call after thread1's async
        // block finished with an error. So it will get none for the key.
        let thread7 = {
            let cache7 = cache.clone();
            spawn(move || {
                // Wait for 400 ms before calling `get`.
                sleep(Duration::from_millis(400));
                let maybe_v = cache7.get(&KEY);
                assert!(maybe_v.is_none());
            })
        };

        // Thread8 will call `get` for the same key. It will call after thread3's async
        // block finished, so it will get the value insert by thread3's async block.
        let thread8 = {
            let cache8 = cache.clone();
            spawn(move || {
                // Wait for 800 ms before calling `get`.
                sleep(Duration::from_millis(800));
                let maybe_v = cache8.get(&KEY);
                assert_eq!(maybe_v, Some("thread3"));
            })
        };

        for t in vec![
            thread1, thread2, thread3, thread4, thread5, thread6, thread7, thread8,
        ] {
            t.join().expect("Failed to join");
        }
    }
}
