//! Which byte of the source each instruction came from.
//!
//! Node reports a line and a column for every frame in a stack trace and for every uncaught error,
//! and a runtime that cannot do that is not a drop in replacement no matter how fast it is. The
//! mapping has to be built while lowering is walking the tree, because that is the only moment when
//! the instruction being emitted and the node it came from are both in hand. Retrofitting it later
//! means guessing, and the guesses are wrong in exactly the places nobody wrote a test for.
//!
//! The table stores a byte offset and not a line and a column. Turning an offset into a line costs
//! one scan of the source and happens when a human is about to read it, which is rare, while storing
//! two numbers per entry would cost memory on every function that is ever loaded, which is not.
//!
//! One entry per run of instructions that share a position, found by binary search. A varint delta
//! encoding would be smaller and is the obvious thing to do when the memory census in spec 08.7 says
//! this table is worth shrinking, but a sorted vector is the version whose lookup is obviously
//! correct, and correctness here is what stack traces are made of.

/// One instruction that starts a new source position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    /// The first instruction covered by this position.
    instruction: u32,
    /// The byte offset in the source file.
    offset: u32,
}

/// The source position of every instruction in one blueprint.
#[derive(Clone, Debug, Default)]
pub struct SourcePositions {
    entries: Vec<Entry>,
}

impl SourcePositions {
    /// Record that the instruction at `instruction` came from byte `offset`.
    ///
    /// Instructions have to be recorded in the order they are emitted, which is what lowering does
    /// anyway. A run of instructions lowered from one expression shares one entry, so a table for a
    /// large function is much smaller than its instruction count.
    pub fn record(&mut self, instruction: usize, offset: u32) {
        let instruction = u32::try_from(instruction).expect("a function fits in u32 instructions");
        match self.entries.last() {
            Some(last) if last.offset == offset => {}
            Some(last) if last.instruction == instruction => {
                // The same instruction index recorded twice means lowering changed its mind about
                // which node an instruction belongs to before emitting it. The later answer is the
                // one closer to the emit, so it wins.
                self.entries.pop();
                self.entries.push(Entry {
                    instruction,
                    offset,
                });
            }
            _ => self.entries.push(Entry {
                instruction,
                offset,
            }),
        }
    }

    /// The byte offset the instruction at this index came from.
    ///
    /// `None` only for an instruction before the first recorded position, which means lowering
    /// emitted something without saying where it came from.
    pub fn offset_at(&self, instruction: usize) -> Option<u32> {
        let instruction = u32::try_from(instruction).ok()?;
        let found = self
            .entries
            .partition_point(|e| e.instruction <= instruction);
        found.checked_sub(1).map(|index| self.entries[index].offset)
    }

    /// How many distinct positions the table holds, which is not the instruction count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::SourcePositions;

    #[test]
    fn an_instruction_gets_the_position_that_was_recorded_for_it() {
        let mut positions = SourcePositions::default();
        positions.record(0, 10);
        positions.record(3, 25);

        assert_eq!(positions.offset_at(0), Some(10));
        assert_eq!(positions.offset_at(3), Some(25));
    }

    #[test]
    fn an_instruction_between_two_entries_belongs_to_the_earlier_one() {
        // This is the whole point of the table. One expression lowers to several instructions and
        // they all report the position of the expression, without an entry each.
        let mut positions = SourcePositions::default();
        positions.record(0, 10);
        positions.record(4, 25);

        assert_eq!(positions.offset_at(1), Some(10));
        assert_eq!(positions.offset_at(3), Some(10));
        assert_eq!(positions.offset_at(4), Some(25));
        assert_eq!(positions.offset_at(99), Some(25));
    }

    #[test]
    fn a_run_of_instructions_at_one_position_is_one_entry() {
        let mut positions = SourcePositions::default();
        for instruction in 0..20 {
            positions.record(instruction, 7);
        }
        assert_eq!(positions.len(), 1);
        assert_eq!(positions.offset_at(19), Some(7));
    }

    #[test]
    fn a_position_that_moves_backwards_is_still_an_entry() {
        // A loop back edge belongs to the top of the loop, which is earlier in the source than the
        // instruction before it, so the table is sorted by instruction and not by offset.
        let mut positions = SourcePositions::default();
        positions.record(0, 5);
        positions.record(1, 40);
        positions.record(2, 5);

        assert_eq!(positions.len(), 3);
        assert_eq!(positions.offset_at(2), Some(5));
    }

    #[test]
    fn nothing_recorded_means_no_answer_rather_than_a_wrong_one() {
        let positions = SourcePositions::default();
        assert!(positions.is_empty());
        assert_eq!(positions.offset_at(0), None);
    }

    #[test]
    fn recording_the_same_instruction_twice_keeps_the_later_answer() {
        let mut positions = SourcePositions::default();
        positions.record(0, 10);
        positions.record(0, 20);

        assert_eq!(positions.len(), 1);
        assert_eq!(positions.offset_at(0), Some(20));
    }
}
