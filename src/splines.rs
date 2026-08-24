// SPDX-License-Identifier: GPL-2.0
//! The other line on the loading screen.
//!
//! Recovery can take a while and there is room on the splash, so there is a
//! line that is not reporting anything. SimCity 2000 has been reticulating
//! splines since 1993 and nobody has improved on it.
//!
//! A function of elapsed time and nothing else: the block redraws every 50ms,
//! so anything picking afresh per call would strobe twenty lines a second. The
//! start is random per process and the step is one, which is enough for two
//! boots to differ and for nothing to repeat within a lap.
//!
//! An entry is a slice, so an entry can be several lines that play out over
//! consecutive periods. Stepping by one gives the ordering for free; the only
//! thing that needs care is starting on an entry boundary, or a boot can open
//! on somebody's punchline.
//!
//! The bar for a new one: it should be worth looking up, and the looking up
//! should be better than the joke. Nothing here is invented - the fungus, the
//! bank, the pitch, the beaver and the disc are all real, and that is the
//! point. See also doc/bcachefs-principles-of-operation.tex, which is the only
//! reference here you can check without leaving the tree.
//!
//! These appear while somebody's filesystem is being repaired after a crash.
//! That is a constraint on tone rather than a reason to be dull: the joke is
//! never at the expense of the person reading it, and never mistakable for a
//! description of what is happening to their data.

use std::sync::OnceLock;
use std::time::Duration;

const PERIOD: Duration = Duration::from_secs(15);

pub fn reticulate(elapsed: Duration) -> &'static str {
    let lines = SPLINES.iter().map(|e| e.len()).sum::<usize>();
    let n = (start() + (elapsed.as_secs() / PERIOD.as_secs()) as usize) % lines;

    SPLINES.iter().flat_map(|e| e.iter()).nth(n).copied().unwrap_or(SPLINES[0][0])
}

/// Flat index of a randomly chosen entry, fixed for the life of the process.
/// An entry rather than a line, so a multi-line one starts at its first line.
/// uuid's v4 is getrandom, and it is already a dependency.
fn start() -> usize {
    static START: OnceLock<usize> = OnceLock::new();

    *START.get_or_init(|| {
        let entry = uuid::Uuid::new_v4().as_u64_pair().0 as usize % SPLINES.len();
        SPLINES[..entry].iter().map(|e| e.len()).sum()
    })
}

/// Add to this. Keep them under about seventy characters - the line is
/// truncated to the console width, and a joke cut in half was not worth the
/// row.
static SPLINES: &[&[&str]] = &[
    &["Reticulating splines"],

    // bcachefs, domesticated. These land because they are true: anyone who has
    // debugged the allocator will recognise having been on the wrong end of a
    // negotiation with it.
    &["Persuading the allocator"],
    &["Composting dead snapshots"],
    &["Eytzingering the search trees"],
    &["Negotiating with copygc"],
    &["Counting buckets twice"],
    &["Reconciling irreconcilable extents"],
    &["Untangling backpointers"],
    &["Asking the btree nicely"],
    &["Apologising to the journal"],
    &["Defragmenting the fragmentation LRU"],
    &["Convincing six locks to agree"],
    &["Bribing the write buffer"],
    &["Rehoming orphaned inodes"],
    &["Explaining snapshots to the extents"],
    &["Interviewing the superblock"],
    &["Sorting bsets by temperament"],
    &["Discarding, eventually"],
    &["Rewriting the remaining C"],
    &["Consulting all four hundred error codes"],
    &["Blaming bcache"],
    &["In the event of curiosity or malfunction, check your PoO"],

    // Refcounting traces dead objects the way tracing traces live ones - the
    // two are duals, which is the whole of Bacon, Cheng and Rajan 2004. Reads
    // as a typo if you have not met the paper and as a thesis if you have.
    &["Refcounting the unreachable"],

    // Armillaria ostoyae: a fungus in Oregon, nine square kilometres of it,
    // somewhere north of two thousand years old, quietly killing trees. The
    // largest known organism on the planet is a thing that eats forests from
    // underneath, which is the correct thing to be checking a btree for.
    &["Checking the btree for Armillaria",
      "Armillaria ostoyae: 9.6 km2, est. 2500 years",
      "Leaving it be"],

    // Banach-Tarski: five pieces, no measure, two balls where there was one.
    // For a filesystem the joke writes itself.
    &["Applying the axiom of choice to free space",
      "Free space decomposed into five non-measurable pieces",
      "Reassembling free space",
      "Reassembling free space again",
      "Free space doubled; please do not ask how"],

    // Knuth pays $2.56 - one hexadecimal dollar - for errors found in his
    // books, and the cheques are drawn on the Bank of San Serriffe, a nation
    // The Guardian invented for April Fool's 1977. It is shaped like a
    // semicolon. Its islands are Upper Caisse and Lower Caisse.
    &["Found an error in Volume 4B",
      "Writing to Professor Knuth",
      "Awaiting one hexadecimal dollar",
      "Cheque drawn on the Bank of San Serriffe",
      "Banking it at Bodoni, Upper Caisse"],

    // Wheeler's one-electron universe, which he phoned Feynman about in 1940:
    // there is one electron, and it goes back and forth through time. Feynman
    // kept the half where positrons are electrons running backwards.
    &["Optimizing multithreading with Feynman diagrams",
      "One worker is running backwards in time",
      "All workers are the same worker"],

    // Kent's, and the arc is the only honest way to finish it.
    &["This would go faster with tachyons, wouldn't it?",
      "Constructing particle accelerator",
      "Particle accelerator constructed",
      "Tachyons remain hypothetical",
      "Dismantling particle accelerator"],

    // The Pitch Drop Experiment, running at Queensland since 1927. Nine drops.
    // John Mainstone kept it for fifty-two years and never once saw one fall.
    // A loading screen has no business being funnier than that.
    &["Waiting for the pitch to drop",
      "Ninth drop fell in 2014",
      "The custodian saw none of them"],

    // Overreach. The joke is scale: something enormously more consequential
    // than your mount, listed as a routine step.
    &["Checking what happens inside black holes"],
    &["Do filesystems dream of electric sheep?"],
    &["Restoring Phorusrhacidae from backup"],
    &["Constructing Turing-complete feedback loop"],
    &["Rebuilding the Library of Alexandria from parity"],
    &["Restoring the biosphere from the Svalbard vault"],
    &["Waiting for the Wow! signal to repeat"],
    &["Querying Voyager 1, round trip 45 hours"],
    &["Computing the sixth busy beaver"],
    &["Enumerating the reals, alphabetically"],
    &["Explaining why alpha is 1/137"],
    &["Dividing by an infinitesimal, rigorously"],
    &["Deciphering Linear A"],
    &["Reading the Phaistos Disc"],
    &["Following the Copper Scroll to the treasure"],
    &["Cross-referencing the Codex Seraphinianus"],
    &["Applying the Antikythera correction"],
    &["Asking Wigner's friend about the cat"],
    &["Determining whether the universe is a filesystem"],
    &["Adjusting for cosmic inflation"],
    &["Locating the missing mass"],
    &["Proving P != NP, briefly"],
    &["Resolving the continuum hypothesis"],
    &["Simulating the heat death for reference"],
    &["Interrogating the anthropic principle"],
    &["Rounding tau to the nearest pi"],

    // Honest to a fault. The display admitting what it is.
    &["Inventing plausible progress"],
    &["Deciding what to tell you"],
    &["Estimating the estimate"],
    &["Refusing to estimate"],
    &["This line is not doing anything"],
    &["Pretending this is faster than it is"],
    &["Doing something useful, honestly"],
    &["Almost done, probably"],
    &["Wondering where the time went"],
    &["Reading the documentation"],
    &["Pretending to have read the documentation"],
    &["Considering the implications"],
    &["Declining to consider the implications"],
    &["Blaming the previous maintainer"],
    &["Being the previous maintainer"],
    &["Looking busy"],
    &["Buffering for dramatic effect"],

    // General purpose.
    &["Herding dirty pages"],
    &["Warming the inodes"],
    &["Feeding the daemons"],
    &["Untangling the Christmas lights"],
    &["Waiting for the spinlock to stop spinning"],
    &["Summoning the maintainer"],
    &["Achieving consensus with itself"],
    &["Politely ignoring the SMART data"],
    &["Applying percussive maintenance"],
];
