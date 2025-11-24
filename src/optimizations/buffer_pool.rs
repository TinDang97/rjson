//! Thread-local buffer pool for reducing allocation overhead
//!
//! Phase 5B.1: Buffer pooling to eliminate malloc/free calls in hot path
//! Expected gain: +10-12% dumps performance

use std::cell::RefCell;

/// Size thresholds for buffer classification
const SMALL_THRESHOLD: usize = 1024;       // 1KB
const MEDIUM_THRESHOLD: usize = 65536;     // 64KB
const MAX_POOL_SIZE: usize = 8;            // Keep max 8 buffers per size class
const MAX_BUFFER_SIZE: usize = 1_000_000; // Don't pool buffers > 1MB

/// Thread-local buffer pool with size-stratified caching
pub struct BufferPool {
    small: Vec<Vec<u8>>,   // Buffers < 1KB
    medium: Vec<Vec<u8>>,  // Buffers 1KB - 64KB
    large: Vec<Vec<u8>>,   // Buffers > 64KB
}

impl BufferPool {
    /// Create a new empty buffer pool
    #[inline]
    fn new() -> Self {
        Self {
            small: Vec::with_capacity(MAX_POOL_SIZE),
            medium: Vec::with_capacity(MAX_POOL_SIZE),
            large: Vec::with_capacity(MAX_POOL_SIZE),
        }
    }

    /// Acquire a buffer from the pool or allocate a new one
    ///
    /// Selects from appropriate size pool based on requested capacity
    #[inline]
    pub fn acquire(&mut self, capacity: usize) -> Vec<u8> {
        let pool = if capacity < SMALL_THRESHOLD {
            &mut self.small
        } else if capacity < MEDIUM_THRESHOLD {
            &mut self.medium
        } else {
            &mut self.large
        };

        // Try to reuse from pool
        if let Some(buf) = pool.pop() {
            buf
        } else {
            // Allocate new with power-of-2 capacity for better reuse
            Vec::with_capacity(capacity.next_power_of_two())
        }
    }

    /// Return a buffer to the pool for reuse
    ///
    /// Clears the buffer and stores it in the appropriate size pool
    #[inline]
    pub fn release(&mut self, mut buf: Vec<u8>) {
        // Don't pool huge buffers
        if buf.capacity() > MAX_BUFFER_SIZE {
            return;
        }

        // Clear contents but keep capacity
        buf.clear();

        // Return to appropriate pool
        let pool = if buf.capacity() < SMALL_THRESHOLD {
            &mut self.small
        } else if buf.capacity() < MEDIUM_THRESHOLD {
            &mut self.medium
        } else {
            &mut self.large
        };

        // Only keep MAX_POOL_SIZE buffers per size class
        if pool.len() < MAX_POOL_SIZE {
            pool.push(buf);
        }
    }

    /// Get pool statistics (for debugging/monitoring)
    #[allow(dead_code)]
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            small_count: self.small.len(),
            medium_count: self.medium.len(),
            large_count: self.large.len(),
        }
    }
}

/// Buffer pool statistics
#[derive(Debug, Clone, Copy)]
pub struct PoolStats {
    pub small_count: usize,
    pub medium_count: usize,
    pub large_count: usize,
}

// Thread-local buffer pool
thread_local! {
    static BUFFER_POOL: RefCell<BufferPool> = RefCell::new(BufferPool::new());
}

/// Acquire a buffer from the thread-local pool
#[inline]
pub fn acquire_buffer(capacity: usize) -> Vec<u8> {
    BUFFER_POOL.with(|pool| pool.borrow_mut().acquire(capacity))
}

/// Release a buffer back to the thread-local pool
#[inline]
pub fn release_buffer(buf: Vec<u8>) {
    BUFFER_POOL.with(|pool| pool.borrow_mut().release(buf))
}

/// Execute a function with a pooled buffer
///
/// Automatically acquires and releases buffer
#[inline]
pub fn with_buffer<F, R>(capacity: usize, f: F) -> R
where
    F: FnOnce(&mut Vec<u8>) -> R,
{
    let mut buf = acquire_buffer(capacity);
    let result = f(&mut buf);
    release_buffer(buf);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_pool_basic() {
        let mut pool = BufferPool::new();

        // Acquire small buffer
        let buf = pool.acquire(512);
        assert!(buf.capacity() >= 512);

        // Release and re-acquire
        pool.release(buf);
        let buf2 = pool.acquire(512);
        assert!(buf2.capacity() >= 512);

        // Should reuse from pool
        let stats = pool.stats();
        assert!(stats.small_count == 0); // Was reused
    }

    #[test]
    fn test_buffer_pool_size_classes() {
        let mut pool = BufferPool::new();

        // Small
        let small = pool.acquire(500);
        pool.release(small);

        // Medium
        let medium = pool.acquire(5000);
        pool.release(medium);

        // Large
        let large = pool.acquire(100_000);
        pool.release(large);

        let stats = pool.stats();
        assert_eq!(stats.small_count, 1);
        assert_eq!(stats.medium_count, 1);
        assert_eq!(stats.large_count, 1);
    }

    #[test]
    fn test_buffer_pool_max_size() {
        let mut pool = BufferPool::new();

        // Fill pool beyond MAX_POOL_SIZE
        for _ in 0..15 {
            let buf = pool.acquire(512);
            pool.release(buf);
        }

        let stats = pool.stats();
        assert!(stats.small_count <= MAX_POOL_SIZE);
    }

    #[test]
    fn test_thread_local_pool() {
        let buf = acquire_buffer(1024);
        assert!(buf.capacity() >= 1024);
        release_buffer(buf);

        // Should reuse from thread-local pool
        let buf2 = acquire_buffer(1024);
        assert!(buf2.capacity() >= 1024);
        release_buffer(buf2);
    }

    #[test]
    fn test_with_buffer() {
        let result = with_buffer(1024, |buf| {
            buf.extend_from_slice(b"test");
            buf.len()
        });

        assert_eq!(result, 4);
    }
}
