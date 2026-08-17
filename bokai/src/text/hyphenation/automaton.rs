//! The matching automaton both dictionary forms compile to.
//!
//! A level is a set of Liang patterns arranged as a trie of byte transitions
//! with fallback links, so one pass over a word finds every pattern that
//! matches anywhere in it. Each state carries the digit string of the pattern
//! that ends there, merged with the digit strings of every shorter pattern
//! ending at the same place, which is what lets a match be read off the state
//! alone rather than by walking the fallback chain at every byte.

/// One state of a level's automaton.
#[derive(Debug, Clone, Copy)]
pub(super) struct State {
    /// Byte offset of this state's digit string in the pool, or `None`.
    pub digits: Option<u32>,
    /// State to retry the current byte in, or `None` to restart from the root.
    pub fallback: Option<u32>,
    /// Where this state's transitions begin in the level's transition array.
    pub trans_start: u32,
    /// How many transitions this state has.
    pub trans_len: u32,
}

/// One level of a dictionary: a pattern set plus the limits it applies.
#[derive(Debug, Clone)]
pub(super) struct Level {
    pub left_min: usize,
    pub right_min: usize,
    pub compound_left_min: usize,
    pub compound_right_min: usize,
    pub states: Vec<State>,
    /// Destination state and matched byte, in state order.
    pub transitions: Vec<(u32, u8)>,
    /// NUL-terminated digit strings, indexed by a state's `digits` offset.
    /// Offset zero means "no digit string", so nothing usable is stored there.
    pub pool: Vec<u8>,
    /// Character sequences that suppress hyphenation next to them.
    pub no_hyphen: Vec<Vec<u8>>,
}

impl Level {
    /// The digit string for a state, as ASCII digits.
    fn digits(&self, state: u32) -> &[u8] {
        let Some(at) = self.states[state as usize].digits else {
            return &[];
        };
        let at = at as usize;
        let end = self.pool[at..]
            .iter()
            .position(|&b| b == 0)
            .map_or(self.pool.len(), |n| at + n);
        &self.pool[at..end]
    }

    /// Run the automaton over `word` and raise `values` wherever a pattern
    /// applies. `values[i]` governs a break before byte `i` of `word`.
    pub fn apply(&self, word: &[u8], values: &mut [u8]) {
        // Patterns are written against a word framed by `.` on both sides, so
        // that a pattern can anchor to the start or the end.
        let mut state: u32 = 0;
        let framed_len = word.len() + 2;
        for i in 0..=framed_len {
            let ch = match i {
                0 => b'.',
                _ if i <= word.len() => word[i - 1],
                _ if i == word.len() + 1 => b'.',
                // One step past the frame flushes any pattern that ends on it.
                _ => 0,
            };
            state = self.step(state, ch);
            let digits = self.digits(state);
            if digits.is_empty() {
                continue;
            }
            // The last digit lands on the byte just consumed, so the string
            // reaches back over the bytes that matched it.
            let Some(start) = (i + 1).checked_sub(digits.len()) else {
                continue;
            };
            for (k, &d) in digits.iter().enumerate() {
                let at = start + k;
                if at < values.len() && values[at] < d - b'0' {
                    values[at] = d - b'0';
                }
            }
        }
    }

    /// The state reached from `state` on `ch`, following fallbacks.
    pub fn step(&self, state: u32, ch: u8) -> u32 {
        let mut state = state;
        loop {
            let s = self.states[state as usize];
            let from = s.trans_start as usize;
            let found = self.transitions[from..from + s.trans_len as usize]
                .iter()
                .find(|(_, c)| *c == ch);
            if let Some(&(next, _)) = found {
                return next;
            }
            match s.fallback {
                Some(f) => state = f,
                // Nothing in the automaton continues this byte; restart.
                None => return 0,
            }
        }
    }
}
