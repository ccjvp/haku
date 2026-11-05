pub struct Bitmap<const N: usize> {
    pub words: [u64; N],
}

impl<const N: usize> Bitmap<N> {
    pub fn new(init: bool, count: usize) -> Self {
        assert!(N != 0);
        assert!(count <= N * 64);

        if !init {
            return Self { words: [0; N] };
        }

        let mut words = [u64::MAX; N];
        let rest = count % 64;

        if rest != 0 {
            words[N - 1] &= (1u64 << rest) - 1;
        }

        Self { words }
    }

    pub fn get(&self, i: usize) -> bool {
        self.words[i / 64] & (1 << (i % 64)) != 0
    }

    pub fn set(&mut self, i: usize, value: bool) {
        let mask = 1 << (i % 64);
        let word = &mut self.words[i / 64];
        if value { *word |= mask } else { *word &= !mask }
    }
}
