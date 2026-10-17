//! Bounded physical-page cache. Owned by one stopped-VM generation.
use crate::{PhysicalMemory, Result};
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct PageCache(RefCell<HashMap<u64, Box<[u8; 4096]>>>);
impl PageCache {
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }
    pub fn pages(&self) -> usize {
        self.0.borrow().len()
    }
}
#[derive(Debug)]
pub struct CachedMemory<'a, M> {
    pub memory: &'a M,
    pub cache: &'a PageCache,
}
impl<M: PhysicalMemory> PhysicalMemory for CachedMemory<'_, M> {
    fn read_physical(&self, address: u64, data: &mut [u8]) -> Result<()> {
        address
            .checked_add(data.len() as u64)
            .ok_or_else(|| crate::Error::Invalid("physical read overflow".into()))?;
        let mut done = 0;
        while done < data.len() {
            let a = address + done as u64;
            let page = a & !4095;
            let off = (a & 4095) as usize;
            let n = (4096 - off).min(data.len() - done);
            let mut cache = self.cache.0.borrow_mut();
            if !cache.contains_key(&page) {
                let mut bytes = Box::new([0; 4096]);
                self.memory.read_physical(page, bytes.as_mut_slice())?;
                // A generation is short lived; bounded wholesale eviction avoids
                // allocating LRU nodes on every page-table walk.
                if cache.len() == 8192 {
                    cache.clear();
                }
                cache.insert(page, bytes);
            }
            data[done..done + n].copy_from_slice(&cache[&page][off..off + n]);
            done += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Memory {
        value: Cell<u8>,
        reads: Cell<usize>,
    }
    impl PhysicalMemory for Memory {
        fn read_physical(&self, _: u64, data: &mut [u8]) -> Result<()> {
            self.reads.set(self.reads.get() + 1);
            data.fill(self.value.get());
            Ok(())
        }
    }
    #[test]
    fn crossing_pages_reuse_and_generation_invalidation() {
        let memory = Memory {
            value: Cell::new(1),
            reads: Cell::new(0),
        };
        let cache = PageCache::default();
        let reader = CachedMemory {
            memory: &memory,
            cache: &cache,
        };
        let mut bytes = [0; 8];
        reader.read_physical(4092, &mut bytes).unwrap();
        reader.read_physical(4092, &mut bytes).unwrap();
        assert_eq!(memory.reads.get(), 2);
        assert_eq!(bytes, [1; 8]);
        memory.value.set(2);
        cache.clear();
        reader.read_physical(4092, &mut bytes).unwrap();
        assert_eq!(memory.reads.get(), 4);
        assert_eq!(bytes, [2; 8]);
        assert!(reader.read_physical(u64::MAX, &mut bytes).is_err());
    }
}
