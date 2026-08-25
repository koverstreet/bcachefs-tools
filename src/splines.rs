// SPDX-License-Identifier: GPL-2.0
//! The other line on the loading screen.
//!
//! Recovery can take a while and there is room on the splash, so there is a
//! line that is not reporting anything. SimCity 2000 has been reticulating
//! splines since 1993 and nobody has improved on it.
//!
//! An entry is a slice, so an entry can be several lines that play out over
//! consecutive periods. Entries are shuffled per process and the lines inside
//! one are not, so a sequence stays a sequence and nothing else is ever seen
//! in the same order twice.
//!
//! Which means position in this array carries no meaning at all. Two entries
//! written next to each other are not seen next to each other; if you want two
//! to land together, they have to be one entry.
//!
//! The bar for a new one is how much argument it starts. Not whether it is
//! correct - correct terminates. The reader gets it, and is done. What you
//! want is the reader stopping to think: why Assyrians, why not Babylonians?
//! Well, Babylonians is not unreasonable - but the Assyrians were the
//! hoarders, so maybe that IS the better place to look. I would have to think
//! about that. That is the reward, and it does not run out on a second
//! reading, which matters on a screen somebody sees for years.
//!
//! So: ridiculous, covering as much ground as possible, and not easily
//! disprovable. The referents are real and the proposition is ours - a fact
//! restated is just a fact, and the reader did not need us for it. Babbage's
//! gears handed to Apollonius, the axiom of choice applied to free space,
//! Alexandria rebuilt from parity. Our verb, their artifact.
//!
//! It also sets the obscurity: too obvious and there is nothing to argue
//! about, too obscure and the reader cannot hold both sides well enough to
//! argue at all. Aim for where somebody knows enough to have an opinion and
//! not enough to be sure.
//!
//! So the fungus, the scroll, the pitch, the beaver and the disc are all real
//! and none of them is the point. See also
//! doc/bcachefs-principles-of-operation.tex, which is the only reference here
//! you can check without leaving the tree.
//!
//! These appear while somebody's filesystem is being repaired after a crash.
//! That is a constraint on tone rather than a reason to be dull: the joke is
//! never at the expense of the person reading it, and never mistakable for a
//! description of what is happening to their data.

use std::sync::OnceLock;
use std::time::Duration;

const PERIOD: Duration = Duration::from_secs(6);

pub fn reticulate(elapsed: Duration) -> &'static str {
    let playlist = playlist();

    playlist[(elapsed.as_secs() / PERIOD.as_secs()) as usize % playlist.len()]
}

/// This process's order: entries shuffled, lines within an entry left alone so
/// a multi-line one still plays out in sequence.
///
/// Shuffling rather than walking the array from a random offset is the point.
/// With an offset, the file's own ordering is the ordering everybody sees -
/// two entries written next to each other are seen next to each other, every
/// boot, forever, and whoever is editing the array is unknowingly editing the
/// running order. Shuffled, position in the file means nothing.
///
/// Still a pure function of elapsed time, which it has to be: the block
/// redraws every 50ms, so drawing afresh per call would strobe twenty lines a
/// second. uuid's v4 is getrandom and already a dependency; one draw seeds the
/// shuffle rather than one per swap.
fn playlist() -> &'static [&'static str] {
    static PLAYLIST: OnceLock<Vec<&'static str>> = OnceLock::new();

    PLAYLIST.get_or_init(|| {
        let mut state = uuid::Uuid::new_v4().as_u64_pair().0 | 1;
        let mut entries = SPLINES.to_vec();

        for i in (1..entries.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            entries.swap(i, (state % (i as u64 + 1)) as usize);
        }

        entries.iter().flat_map(|e| e.iter().copied()).collect()
    })
}

/// Add to this. Keep them under about seventy characters - the line is
/// truncated to the console width, and a joke cut in half was not worth the
/// row.
///
/// There are three ways a line can pay, and they are not ranked. Some point
/// at something real and the reward is going and finding it out. Some are
/// assembled instead: no search finds "Probing CMB for messages to Susskind"
/// or the Babbage one, because the pieces are already in the reader and the
/// work is putting them together. And some are just funny, which is a whole
/// mode and not a lesser one - see the honest-to-a-fault block.
///
/// Multi-line entries cost less than they used to but still cost: a payload
/// lands a PERIOD after its setup and something has to carry the reader
/// across. Usually the second line refers back by itself - "the copy
/// filmed off a monitor" cannot be about anything but the tapes. It can also
/// bind through the lookup: nobody connects "SCE to AUX" to a steely-eyed
/// missile man until they go and find out, and then it arrives welded. What
/// fails is a payload that does neither - a bare "1202" reaches nobody it had
/// not already reached.
static SPLINES: &[&[&str]] = &[
    &["Reticulating splines"],

    // bcachefs, domesticated. These land because they are true: anyone who has
    // debugged the allocator will recognise having been on the wrong end of a
    // negotiation with it.
    &["Persuading the allocator"],
    &["Composting dead snapshots"],

    // Kent's, and it must stay a SEPARATE ENTRY from "Restoring
    // Phorusrhacidae from backup" - never merged into one. The shuffle then
    // spaces them at random, which is the joke: you meet one, and much later
    // you meet the other, and the program has advanced in between. Nobody
    // comments on it. As one entry they would always play together and it
    // would just be a two-line gag.
    //
    // Restoring an extinct apex predator is a joke about scale. Tuning its
    // hunting is a joke about it being routine.
    &["Optimizing Phorusrhacidae hunting efficiency"],

    // Michael Eytzinger, Thesaurus principum, 1590: number a person n, their
    // father 2n, their mother 2n+1. That is the implicit binary tree, and a
    // genealogist had it four hundred years before anyone needed it to be
    // cache friendly. Khuong and Morin gave the layout its name and its
    // analysis in 2015. bcache was in Linux 3.10, in 2013.
    //
    // The dates are simply stated in the order that makes a reader stop, and
    // nothing claims anything - "Linux 3.10 shipped it in 2013" does not say
    // bcache, or who, or first. Anyone who wants to know goes and finds out,
    // and the finding out is the whole point; said out loud it would be
    // insufferable.
    //
    // The last line is Kent's and it is the whole thing. eytzinger1_to_inorder()
    // is what makes the layout usable in place with no side table - mostly
    // shifts - and it is in this tree and in no paper. Stated plainly that
    // reads as a complaint. As a status field it does not: the order is
    // supposed to be hypothesise, publish, ship, and this one skipped the
    // middle. The claim is never made, so it cannot be insufferable.
    &["Eytzingering the search trees",
      "Father at 2n, mother at 2n+1. A pedigree chart, 1590",
      "Named and analysed for cache-friendly search in 2015",
      "Linux 3.10 shipped it in 2013",
      "eytzinger1_to_inorder(): hypothesized but shipping"],
    &["Negotiating with copygc"],
    &["Counting buckets twice"],
    &["Reconciling irreconcilable extents"],
    &["Untangling backpointers"],
    &["Asking the btree nicely"],

    // Kent's shape, rooted in Godel where it belongs. The second
    // incompleteness theorem says a consistent system cannot prove its own
    // consistency - and checking consistency is the entire job of the thing
    // this line appears during. So it is asking, from inside, for the one
    // proof that provably cannot be had from inside.
    //
    // "Sign off on" is deliberately wrong for the register, and that is
    // Kent's point about "resolve recursion": the machine not quite knowing
    // the vocabulary for what it wants is half the joke. It has gone looking
    // for a rubber stamp and picked the one man who will explain why there
    // isn't one.
    &["Asking Hofstadter to sign off on our consistency"],
    &["Apologising to the journal"],
    &["Defragmenting the fragmentation LRU"],
    &["Convincing six locks to agree"],
    &["Bribing the write buffer"],
    &["Sending orphaned inodes to a good home"],
    &["Explaining snapshots to the extents"],
    &["Interviewing the superblock"],
    &["Sorting bsets by temperament"],
    &["Discarding, eventually"],
    &["Rewriting the remaining C"],
    &["Consulting all four hundred error codes"],
    &["Blaming bcache"],

    // Kent's, and it lands here because twenty lines of apologising to the
    // journal and bribing the write buffer have just gone past, and then the
    // filesystem puffs its chest out. Katsuobushi is skipjack simmered,
    // smoked for weeks, inoculated with Aspergillus and sun-dried over
    // months until it is the hardest food in the world; you shave it with a
    // carpenter's plane. Hardened deliberately, by a controlled process,
    // over a very long time. The exclamation mark is load bearing - it makes
    // the line an advertisement rather than a claim.
    &["bcachefs - harder than dried bonito!"],

    // btree_node_cannibalize() is the real name of a real function
    // (fs/btree/cache.c:917), and there is a real cannibalize lock
    // serialising it, because two threads eating the cache at once
    // deadlock. A stranger assumes we made the word up for the splash.
    &["Cannibalizing the btree cache",
      "Only one thread may do this at a time"],

    // fs/fs/check.c:2897 - mustfix_fsck_err_on(!S_ISDIR(root_inode.bi_mode),
    // ..., "root inode not a directory"). A real error code with a real
    // repair arm. Every other one-liner here runs on personification; this
    // one is a tautology solemnly performed, which is a mechanism the file
    // did not have.
    &["Confirming the root directory is a directory"],
    // Kent's, both lines. The first was already here; the second is what it
    // was missing, and it is the joke - the Principles of Operation lives in
    // doc/ on the filesystem currently being repaired, so the one moment you
    // need it is the one moment you cannot open it. Advice that eats itself,
    // delivered helpfully, by the thing that ate it.
    &["In the event of curiosity or malfunction, check your PoO",
      "You did print it, right?"],

    // Refcounting traces dead objects the way tracing traces live ones - the
    // two are duals, which is the whole of Bacon, Cheng and Rajan 2004. Reads
    // as a typo if you have not met the paper and as a thesis if you have.
    &["Refcounting the unreachable"],

    // Armillaria ostoyae: a fungus in Oregon, nine square kilometres of it,
    // somewhere north of two thousand years old, quietly killing trees. The
    // largest known organism on the planet is a thing that eats forests from
    // underneath, which is the correct thing to be checking a btree for.
    &["Checking the btree for Armillaria",
      "Leaving it be"],

    // Banach-Tarski: five pieces, no measure, two balls where there was one.
    // For a filesystem the joke writes itself.
    // Kent's, and played completely straight - it is a procedure being
    // reported, not a joke being told. The ellipses are load bearing: they
    // make it real progress output. The earlier version ended "please do not
    // ask how", which was a wink, and the wink is what killed it. Here the
    // machine does not think anything has gone wrong.
    &["Applying Axiom of Choice to free space according to Banach Tarski...",
      "Computing five partitions...",
      "Free space doubled"],

    // Not the $2.56 cheque - everybody has heard that one, so it does no
    // work. The Knuth that touches this codebase is TAOCP Vol 3, 6.2.1, on
    // the algorithm running in a bset a few million times a second.
    //
    // Binary search is published in 1946. The first version correct for all
    // n, rather than only n = 2^k - 1, appears in 1962. Sixteen years, for
    // something that fits on a napkin, and Knuth's own remark is that the
    // details are surprisingly tricky. Bentley later found roughly one in ten
    // professional programmers could write it correctly given hours. Then in
    // 2006 Bloch found (low + high) / 2 overflowing - in the JDK's
    // Arrays.binarySearch, and in the printed Programming Pearls. Sixty years
    // from publication to the canonical version still being wrong.
    //
    // The last line is ours and it is true: our answer was not a better
    // binary search, it was to permute the array so the accesses land where
    // the cache wants them. Anyone who follows it arrives at
    // fs/util/eytzinger.h and learns that Eytzinger was a 16th century
    // genealogist laying out pedigrees, which is the delight.
    &["Binary searching the bset",
      "Published 1946. Correct for all n: 1962",
      "Knuth, Vol 3: the details are surprisingly tricky",
      "Bloch found (lo+hi)/2 still overflowing in the JDK, 2006",
      "Sixty years. We reordered the array instead"],

    // Wheeler's one-electron universe, which he phoned Feynman about in 1940:
    // there is one electron, and it goes back and forth through time. Feynman
    // kept the half where positrons are electrons running backwards.
    &["Optimizing multithreading with Feynman diagrams",
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
      "The custodian saw none of them"],

    // Thompson's Turing lecture, 1984. He put a backdoor in login, then put
    // the backdoor in the compiler so it would reinsert itself into a clean
    // login, then put *that* in the compiler so it survived recompiling the
    // compiler from clean source. The source is clean. The binary is not.
    // Nobody has a good answer thirty years on.
    &["Verifying the compiler",
      "Compiling the compiler with itself",
      "You cannot trust code you did not totally create yourself",
      "Trusting it anyway"],

    // The single best thing in the file, and it is not close. A 10th century
    // copy of Archimedes was scraped down in 1229 and overwritten with a
    // prayer book; X-ray fluorescence pulled the undertext back in the 2000s
    // and it was the only surviving copy of The Method, in which Archimedes
    // does integral calculus in 250 BC. Somebody deleted calculus for
    // eighteen centuries to reuse the media.
    //
    // Kent's line, and it is the model for the four below it: our verb on
    // their artifact. Four lines narrating the story was me doing the
    // reader's lookup for them, worse than the lookup does it, and spending
    // a minute of screen time to be worse. One line points; the archive
    // pays out.
    &["Testing new data recovery methods on the Archimedes Palimpsest"],

    // The BBC's Domesday Project, 1986: the nation surveyed, on LaserDisc,
    // readable only by a BBC Micro with a specific drive. Effectively
    // unreadable within fifteen years, and it took a rescue project to get it
    // back. The Domesday Book it was named for is from 1086 and you can go
    // and read it. Nine hundred years of parchment beat fifteen of optical.
    &["Migrating the Domesday Project off LaserDisc"],

    // The Hochrheinbrücke at Laufenburg, built from both banks at once. The
    // German datum is referenced to the North Sea and the Swiss to the
    // Mediterranean, 27 cm apart; the engineers knew, applied the correction,
    // and applied it with the wrong sign, so the halves met 54 cm out. Two
    // authorities, both internally consistent, disagreeing about zero - and
    // "reconcile" is our word for exactly that, which is why the verb does
    // the work here rather than the anecdote.
    &["Reconciling the two sea levels at Laufenburg"],

    // Sweden converted to Gregorian by planning to skip every leap day from
    // 1700 to 1740. It skipped 1700, went to war, observed 1704 and 1708 out
    // of habit, and was then aligned with nobody at all. In 1712 it gave up
    // and reverted to Julian by spending the accumulated difference at once:
    // that February had thirty days. A migration abandoned halfway and rolled
    // back by writing the delta into the data.
    &["Rolling back to February 30th"],

    // Ise Jingu has been rebuilt every twenty years since 690, on one of two
    // adjacent sites, alternating - sixty-two times so far. The old shrine
    // stands until the new one is finished and consecrated, and only then is
    // it taken down. Copy on write, two slots, thirteen hundred years of
    // uptime, and the reason the carpentry survives is that it is exercised
    // rather than preserved.
    &["Rebuilding Ise on the other site"],

    // Sealand: a WWII sea fort declared a sovereign principality in 1967.
    // HavenCo ran an offshore data haven from it beyond anyone's
    // jurisdiction. The closest anyone came to building the thing.
    &["Replicating to Sealand"],

    // The one Dead Sea scroll written on metal, because it was meant to last.
    // Sixty-four locations, tonnes of gold and silver, and nothing has ever
    // been found at any of them. Somebody engraved the index to outlast us
    // and did not write down where the data was: sixty-four pointers, all of
    // them dangling, still perfectly legible after two thousand years.
    &["Dereferencing the Copper Scroll"],

    // Everyone knows the headline, so the headline cannot be the joke: the
    // ground software produced impulse in pound-force seconds, navigation
    // expected newton-seconds, neither was internally wrong, and the
    // interface carried the number without the unit. Which is what an on-disk
    // format is, and why ours is versioned.
    //
    // The part people do not know is the shape of it. Not one bad burn - every
    // angular momentum desaturation across four and a half months of cruise,
    // each off by the same 4.45, integrating. And the navigation team saw the
    // trajectory pulling consistently one way and it was put down to other
    // causes. The spacecraft was reporting the bug the entire trip.
    //
    // So it ends on that and not on the crash, because a small persistent
    // discrepancy that might be real and might be noise is exactly the
    // judgement the reader's filesystem is making while this is on screen.
    &["Converting Mars Climate Orbiter to Metric"],

    // Ariane 5, flight 501. A guidance routine carried over from Ariane 4,
    // serving no purpose after liftoff, converted a 64-bit float to a signed
    // 16-bit int. Ariane 5 flew faster than Ariane 4, so it overflowed. Both
    // redundant units ran the same dead code and failed identically.
    &["Removing what the previous version needed",
      "Ariane 501: an Ariane 4 routine, unused after liftoff",
      "64 bits into 16, on both redundant units",
      "Thirty-nine seconds"],

    // The best bug report ever filed. Dwarf Fortress cats were dying of
    // alcohol poisoning: spilled booze accumulated on their paws, cats clean
    // their paws, and the drink was charged as one full unit of liver damage
    // regardless of how little was actually there. An accounting bug.
    &["Investigating why the cats keep dying",
      "Spilled alcohol accumulates on paws; cats groom",
      "Each drop charged as one whole drink"],

    // Kent's. Apollonius had the mathematics and no machine; Babbage had the
    // machine and never finished one. Stated as a routine step, and the
    // reader assembles the rest.
    //
    // The part underneath is that the heist is unnecessary: Antikythera is
    // within a century of Apollonius, and it is a differential gear train.
    // They had the gears. It went nowhere.
    &["Stealing Babbage's gears and giving them to Apollonius"],

    // Compressed, deliberately: narrated, each of these was a lecture that
    // handed the reader its own conclusion. One line each, real referent,
    // and the work of unpacking left where it belongs. Roughly half are
    // framed with an impossible or playful verb and half are flat; the flat
    // ones are the machine calmly announcing it is about to do the thing
    // that historically caused the catastrophe.
    //
    // B-trees: Bayer and McCreight 1972, and McCreight would never say
    // whether the B was Boeing, balanced, broad, bushy or Bayer.
    &["Asking what the B stands for"],

    // Hubble: two tests said the mirror was wrong, the third was more
    // precise, the third won, and the third was the one miscalibrated.
    &["Asking the third instrument to reconsider"],

    // Millennium Bridge, 2000: the sway made walkers match steps, and
    // matching steps made the sway. Closed after two days.
    &["Waiting for everyone to fall out of step"],

    // Trey Harris: a campus that could not send mail more than five hundred
    // miles, because a zeroed timeout left only the connect round trip, and
    // three milliseconds of light is about five hundred and sixty miles. Not
    // in fibre - that would be three hundred and eighty - Harris did the sum
    // in vacuum and the number he got is the one in the title.
    &["Checking whether the timeout is in miles"],

    // The Mark II moth, 1947, is captioned "First actual case of bug being
    // found". Actual - the word was already old. That is the joke, and it
    // is why they kept the moth.
    &["Extracting the actual bug"],

    // Y2K: hundreds of billions of dollars, mostly COBOL, and it worked so
    // well that everybody concluded there had never been a problem.
    &["Saving two digits on the year"],

    // The Apollo 11 SSTV originals - better than anything broadcast - were
    // degaussed during a 1980s tape shortage. They looked like blank stock.
    // Two lines, because the second one IS the joke and collapsing to one
    // threw it away: setup, payload, nothing narrated in between.
    &["Erasing the tapes to make room",
      "Keeping the copy filmed off a monitor"],

    // Apollo 12, struck by lightning twice inside a minute: telemetry to
    // garbage, and John Aaron called an obscure switch he had chased down in a
    // test a year earlier for no reason at all. Alan Bean was the only man
    // aboard who knew where it was. The title is NASA's highest informal
    // honour and not a post anyone can apply for.
    &["Trying SCE to AUX",
      "Promoting a steely-eyed missile man"],

    // The Cairo Geniza: nothing bearing the Name may be destroyed, so it
    // went in a room instead, for a thousand years. Three hundred thousand
    // fragments, including somebody's alphabet homework.
    &["Filing what may not be deleted"],

    // Nineveh, 612 BC. The tablets were unfired clay and the fire that
    // levelled the palace baked them hard. The catastrophe is the reason
    // there is anything left to read.
    &["Preserving the archive by burning the palace"],

    // Same library, and now we are mining it. Ashurbanipal sent agents into
    // Babylonia to strip the temple collections, so Nineveh really is a
    // general-purpose corpus somebody hoarded on the off-chance - grepping
    // it is not even unreasonable.
    //
    // "Grepping" because we have not found it. Not something they had -
    // something there is no reason they could not have had.
    //
    // Both halves are arguable and neither is settled here, which is the
    // point. Could they? They interpolated between tabulated values as
    // routine practice and kept enormous sorted tables, so the step from
    // interpolating a value to interpolating a position is small - but
    // nobody has found the tablet. Would it help? Binary search is only
    // optimal in the comparison model and bset keys are near-uniform - but
    // eytzinger already fixed the part that actually hurt, which was cache
    // misses rather than comparison count, so possibly not at all.
    //
    // Two people who get it can disagree about it. That is what keeps a line
    // alive on a screen somebody has already read a hundred times, and it is
    // worth more than being right.
    &["Grepping Assyrian tablets for better binary search"],

    // When a unit left Vindolanda it cleared out the obsolete paperwork and
    // burned it in the courtyard. The tablets are visibly charred; Bowman's
    // reading is that the fire was probably put out by rain before it got
    // through them. A delete that was issued and did not complete - so the
    // discard is still pending, and we are retrying it nineteen centuries
    // later. What they failed to destroy includes a birthday invitation and
    // a soldier writing home about socks.
    //
    // Strictly the corpus survived because each fort phase was sealed under
    // clay below the water table, and Bowman hedges the rain twice. Does not
    // matter: the line does not claim the fire is why they survived, only
    // that somebody tried to delete them and it did not take. That part is
    // flatly true and is the whole joke.
    &["Retrying the discard at Vindolanda"],

    // Herculaneum, carbonised in 79 and far too fragile to unroll at all.
    // Tomography and a machine learning prize got a word out of a scroll
    // nobody had opened, in 2023, and after two thousand years it was
    // "purple". The setup had to name the word for "It" to have anything to
    // attach to a period later.
    &["Reading the first word off the burnt scroll",
      "It was 'purple'"],

    // Arecibo, 1974: 1679 bits, and 1679 is 23 x 73, both prime - so there
    // are only two ways to fold it into a rectangle and only one is not
    // noise. The dimensions ride in the length.
    &["Choosing a prime number of bits"],

    // Krauss and Scherrer, "The Return of a Static Universe and the End of
    // Cosmology" (Gen Rel Grav 39:1545, 2007): within under fifty times the
    // present age of the universe, expansion redshifts the CMB below the
    // ~1 kHz plasma frequency of our own galaxy's ionised gas, which then
    // screens it out completely. The evidence for the Big Bang has an expiry
    // date, and after that the observable universe looks static and eternal
    // to anyone inside a galaxy. An archival deadline, which is our problem
    // exactly, on the only copy there is.
    &["Archiving the CMB before the galaxy screens it out"],

    // Kent's, and it is assembled rather than looked up. If universes are
    // born inside black holes, the parent's only channel into the child is
    // whatever is imprinted at the very start - which is this. So a message
    // from outside arrives here, in the oldest light there is. And if it is
    // addressed to one person, the recipient is whoever has spent a career
    // working out what it is like inside a black hole: Susskind is studying
    // his own address without knowing it, and someone out there is writing
    // to him at it.
    //
    // No search finds this, and looking for one is a category error - the
    // pieces are already in the reader. Article dropped so it scans as a
    // status line, like "Constructing particle accelerator".
    //
    // Adjacent and real, for anyone who wants the literature: Hsu and Zee,
    // "Message in the Sky" (Mod Phys Lett A 21:1495, 2006) propose the CMB
    // as the Creator's billboard, needing only a tuned Lagrangian and no
    // intervention afterwards. Hippke later decoded WMAP and Planck into a
    // bitstream and found nothing in it.
    &["Probing CMB for messages to Susskind"],

    // The autoclave standard is 121 C for fifteen minutes because that was
    // taken to kill everything. Kashefi and Lovley (Science, 2003) pulled
    // Strain 121 off a Juan de Fuca vent; it reproduces at exactly 121 C, and
    // 130 C only stops it - transfer it back to 103 C and it recovers. The
    // test that defines sterile has an organism living at its pass mark,
    // which is a failure mode this file's author has some sympathy for.
    &["Autoclaving at exactly Strain 121's optimum"],

    // Knight Capital, 2012. Forty-five minutes, four hundred and sixty
    // million dollars, and a flag reused from a feature retired in 2003.
    &["Deploying to seven of the eight"],

    // August 2003: the alarm subsystem raced and stopped, and did not
    // report that it had stopped. Fifty-five million people.
    &["Checking that the alarm system is alarming"],

    // Overreach. The joke is scale: something enormously more consequential
    // than your mount, listed as a routine step.
    &["Checking what happens inside black holes"],

    // Kent's, and it is the register the whole block was reaching for. Stross's
    // Eschaton scattered humanity across the galaxy and left a note saying it
    // is not your god, it is descended from you, and thou shalt not violate
    // causality within its historic light cone. You do not ask it for things.
    // The filesystem asks it for a bit more compute, the way you would raise a
    // ticket. No article, so it reads like a vendor.
    &["Asking Eschaton for more compute"],

    // Kent's, and the flatness is the joke. Simmons' Shrike moves through
    // time, which means it can go back and get the thing that was lost - so
    // it is, technically, the best data recovery tool ever described. We are
    // training it. "Recover data" is deliberately corporate: an unkillable
    // blade monster from the far future, put on the support desk. No article,
    // so it reads like something with a name that answers to it.
    &["Teaching Shrike to recover data"],

    // Kent's line above is about the inside; this is about the books. A 10
    // solar mass hole has a Hawking temperature of 6e-9 K and sits in a 2.725 K
    // bath, so it absorbs far more than it emits: every black hole known to
    // astronomy is currently GAINING mass. Break-even is 4.5e22 kg, about
    // 0.6 lunar masses, and net evaporation cannot begin for something like
    // 1e11 years. Framed as a deferral because that is exactly what it is -
    // the work is queued behind a condition that has not been met yet.
    &["Deferring evaporation until the CMB cools"],
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

    // And its neighbour, which is why it is allowed to repeat the referent.
    // Eddington derived the exact number of protons in the universe as
    // 136 x 2^256, on the grounds that alpha was then 1/136. When alpha was
    // remeasured nearer 1/137 he revised the derivation to 137 x 2^256 - so
    // the correction added exactly 2^256 protons to the universe.
    &["Adding 2^256 protons to the universe"],
    &["Dividing by an infinitesimal, rigorously"],
    &["Deciphering Linear A"],

    // Stamped with punches - movable type, three thousand years before
    // Gutenberg - and undeciphered. There is exactly one of them: no second
    // copy, nothing to check it against, no way to know if it is even a
    // language. One replica, no parity, and the checksum is unknowable.
    &["Checksumming the Phaistos Disc"],

    &["Cross-referencing the Codex Seraphinianus"],
    &["Applying the Antikythera correction"],
    &["Asking Wigner's friend about the cat"],
    &["Resolving the continuum hypothesis"],
    &["Surveying the Bootes void, 330 Mly of nothing"],

    // Honest to a fault. The display admitting what it is - and the only
    // place in the file where a joke is allowed to have nothing to look up,
    // so they have to be funny on the line alone.
    //
    // The pairs are one entry each rather than two adjacent ones. start()
    // lands on an entry boundary, so as two, roughly one boot in a hundred
    // opens on the second half with nothing in front of it.
    &["Inventing plausible progress"],
    &["Deciding what to tell you"],
    &["Estimating the estimate",
      "Refusing to estimate"],

    // Hofstadter's Law performed rather than quoted, which is the only way
    // to use it - the quote is too famous to do any work. The gap is the
    // joke: the second line arrives later than you expected.
    &["Taking Hofstadter's Law into account",
      "Taking that into account"],
    &["This line is not doing anything"],
    &["Doing something useful, honestly"],
    &["Reading the documentation",
      "Pretending to have read the documentation"],
    &["Considering the implications",
      "Declining to consider the implications"],
    &["Blaming the previous maintainer",
      "Being the previous maintainer"],
    &["Buffering for dramatic effect"],

    // General purpose.
    &["Untangling the Christmas lights"],
    &["Summoning the maintainer"],
    &["Herding dirty pages"],
    &["Warming the inodes"],
    &["Feeding the daemons"],
    &["Waiting for the spinlock to stop spinning"],
    &["Achieving consensus with itself"],

    // Kent's, and it is the whole entry. Every other line in the file is a
    // verb doing something to an object; this one has given up on that and
    // is just the animal's name. It is also a real taxonomic name, which is
    // the part that arrives second.
    &["Bison bison bison"],
    &["Politely ignoring the SMART data"],
    &["Applying percussive maintenance"],
];

// ---------------------------------------------------------------------------
// The graveyard.
//
// Everything cut, and why. This exists because pruning without it is lossy in
// exactly one direction and the loss is invisible at the time you take it: a
// whole night's work was thrown away here by treating each new note as an
// instruction to rewrite rather than as a lens to re-see what was already
// good. Twice the objection was to an entry's SETUP and the payload went in
// the bin with it.
//
// So: nothing leaves this file. Cut it from SPLINES, paste it here with one
// line on why. A later reader gets the rejects and the reasons instead of
// rediscovering both.
//
// DO NOT USE. Every one of these is a wonderful fact that is not true, or not
// true in the form that makes it wonderful. They are recorded because they
// are exactly the ones a future contributor will reach for, and because this
// file's whole promise is that a reader who looks something up is rewarded
// rather than misled. Checked 2026-08-24.
//
//   Arsenic life / GFAJ-1. RETRACTED. The 2010 claim of arsenate in the DNA
//   backbone was refuted in 2012 by two independent groups (Reaves et al.,
//   Erb et al., both Science 337) and Science retracted the paper in July
//   2025. GFAJ-1 is real, is from Mono Lake, and is genuinely
//   arsenate-resistant - it just needs phosphate like everything else.
//
//   The 250-million-year-old Permian salt bacterium. CONTESTED and never
//   independently replicated. Vreeland et al. (Nature 407:897, 2000) against
//   Graur and Pupko, "The Permian Bacterium that Isn't" (MBE 18:1143) - its
//   16S differs from a modern Dead Sea organism by two bases in 1,555, with
//   no branch slowdown. Open as of 2026, so it cannot be stated flat.
//
//   "The animal that doesn't breathe." Henneguya salminicola really has no
//   mitochondrial genome (Yahalomi et al., PNAS 117:5358) - but how it makes
//   ATP is unknown and anaerobic respiration is not excluded. The precise
//   version is still good; the popular version is an overclaim.
//
//   "Bacterial ice nucleators freeze water better than any known material."
//   The literature says "unmatched among a wide variety of heterogeneous ice
//   nucleators", which is weaker. The -2 C figure and Snomax are fine.
//
//   "The relic neutrino background has never been detected." False as
//   phrased - it is detected in aggregate, via the free-streaming phase
//   shift in the CMB peaks (~10 sigma). Only "not one has ever been detected
//   individually" is exact, and that qualifier is the whole line.
//
// CUT AND SHOULD STAY CUT - puns with no referent, or gestures at a field
// rather than a fact. Nothing to look up, so nothing to be delighted by.
//   "Rounding tau to the nearest pi"
//   "Proving P != NP, briefly"
//   "Adjusting for cosmic inflation"
//   "Locating the missing mass"
//   "Simulating the heat death for reference"
//   "Interrogating the anthropic principle"
//   "Determining whether the universe is a filesystem"
//
// CUT BY MISTAKE, RESTORED - voice, lands cold, no facts required. Removed as
// "generic" during a sweep that simultaneously added thirteen sequences.
//   "Untangling the Christmas lights"
//   "Summoning the maintainer"
//
// PAYLOADS BINNED WITH THEIR SETUPS - each of these was the actual joke, lost
// when the entry around it was collapsed to a one-liner. Five have since gone
// back, which is the only evidence this section works, so they are listed
// rather than quietly dropped:
//   "Domesday Book, 1086: still fine"    RESTORED
//   "It is an index"                     RESTORED
//   "There is no second copy"            RESTORED
//   "He had integration in 250 BC"       RESTORED
//   "a camera pointed at a monitor"      RESTORED, as "the copy filmed off
//                                        a monitor"
//
//   "It was 'purple'"                    RESTORED, and the fix was the setup
//                                        rather than the payload - "It" had
//                                        nothing to attach to after
//                                        "Carefully unrolling the carbonised
//                                        scroll", so the setup now names the
//                                        word it is talking about
//
// Still out, and each fails the binding rule, which is why:
//   "and someone's homework" Cairo Geniza. Needs the 300,000 fragments in
//                            between to land, so it is a three-liner or
//                            nothing.
//   "It read as noise"                   MCO: four months of consistent drift
//   "The third was out of calibration"   Hubble: two tests said no, the precise one won
//   "The fire is why we can read them"   Nineveh - now redundant, the
//                            one-liner already carries it
//
// CUT AS DUPLICATE REFERENTS. Four subjects had two entries each, which the
// accumulate phase cannot see and a prune pass is exactly for. In each case
// the survivor is the one that carries a fact rather than a gesture, and the
// Mars one is the whole failure mode in miniature: Kent's line was added as
// an improvement on mine and mine was never taken out, so the file shipped
// both for a week.
//   "Recovering the Mars Climate Orbiter's units"   vs "Converting Mars
//                                                   Climate Orbiter to Metric"
//   "Following the Copper Scroll to the treasure"   vs the four-line entry
//                                                   ending "It is an index"
//   "Dating the Antikythera mechanism's missing gear" vs "Applying the
//                                                   Antikythera correction"
//   "Explaining the 1977 Wow! signal to the extents" vs "Waiting for the Wow!
//                                                   signal to repeat" - and it
//                                                   also reran the "explaining
//                                                   X to the extents" frame
//
// CUT AS NOT FUNNY ENOUGH TO CARRY NOTHING. The honest-to-a-fault block is
// the one place with no referent to look up, so the line has to be funny by
// itself. These three were only wry, and two of them make a claim about
// progress that this screen should not be making:
//   "Almost done, probably"
//   "Pretending this is faster than it is"
//   "Wondering where the time went"
//
// CUT BY CONFLATION - the objection was that everyone knows Knuth's $2.56
// bounty, which is true and was about the SETUP. The payload was never the
// bounty: San Serriffe's islands migrate, so the bank holding your money is
// at an address that moves. That is copygc. Wants to return without the
// bounty in it at all.
//   "Cheque drawn on the Bank of San Serriffe"
//   "Banking it at Bodoni, Upper Caisse"
//
// UNRANKED, CUT AS "TRIVIA", WORTH RE-ARGUING - dismissed fast and at least
// the first two have whimsy I did not credit.
//   thagomizer      a Far Side joke, 1982, now genuine palaeontological usage
//   RFC 1149        avian carriers; Bergen actually ran it, 5222s round trip
//   Vasa            two crews, two rulers, twelve inches against eleven
//   Kryptos K4      97 characters, unsolved since 1990, in the CIA courtyard
//   Long Now clock  ten thousand years, ticks once a year
//   Beresheet       several thousand tardigrades, impact survivability unknown
//
// THE POOL - worked but not yet placed. The graveyard runs forwards too:
// candidates live here rather than in a chat log, so the next pass inherits
// the thinking instead of redoing it.
//
// APOLLO. The erased-tapes entry is in; the programme has far more, and the
// good ones are all about a system under load doing the right thing.
//
//   1202. Best of them, because it is the same situation as the screen it
//   would appear on. On the descent the AGC threw executive overflow: the
//   rendezvous radar had been left in a mode that stole ~15% of its cycles.
//   It did not crash. Hamilton's scheduler restarted, dropped the
//   low-priority jobs and kept the landing. The alarm WAS the system working,
//   which is exactly how a recovery pass looks to a frightened user.
//       "Shedding the low-priority jobs"
//       "1202"
//   ...and that form fails the binding rule, though not for the reason first
//   written here. The defect is the bare "1202", which nobody can search, not
//   the second line as such. Make the payload findable and it binds through
//   the lookup exactly as the steely-eyed missile man does:
//       "Shedding the low-priority jobs"
//       "Program alarm 1202"
//   or as one line, "Calling GO on program alarm 1202".
//   Steve Bales had about thirty seconds to make that call, and made it off a
//   sheet of survivable alarm codes Jack Garman had written out by hand
//   because Kranz had drilled them on program alarms after one bad sim. Same
//   moral as Aaron's - the obscure preparation nobody asked for - which is an
//   argument for spacing the two, not for cutting either.
//
//   Rope memory. AGC programs were woven - wire through a core for one,
//   around it for zero - by hand, by women at Raytheon. Immutable once made,
//   and the software had to freeze months before flight because weaving took
//   weeks. Read-only memory that is physically read-only.
//       "Weaving the firmware"
//
//   SCE to AUX - PROMOTED, as a two-liner. Kept here because the move is the
//   whole point. The objection was mine: superb, but too inside-baseball to
//   land cold. The steely-eyed missile man answers it without touching the
//   story - the title is delightful at zero knowledge, and Aaron and the
//   lightning become the reward for looking it up rather than the price of
//   admission. It never needed replacing. It needed a door.
//
//   The first draft of the binding rule above then said the two-line form
//   could not work, since nothing in the payload points back at "SCE to AUX".
//   Wrong, and instructively so: the lookup points back. That is where the
//   rule's second clause came from, and it was worth more than the entry.
//
//   Retroreflectors. Apollo 11, 14 and 15 left corner cubes on the surface.
//   They are the only Apollo experiment still returning data - you can still
//   range them. Passive, no power, answering queries for fifty-seven years.
//       "Ranging the Apollo 11 retroreflector"
//       "It still answers"
//
//   Felt-tip pen. Aldrin broke the ascent engine arming breaker with his
//   backpack and jammed it closed with a pen. Repair with what is to hand.
//       "Jamming the breaker with a felt-tip pen"
//
//   Cold start. Apollo 13 powered the command module up from dead, which had
//   never been done and was believed impossible; Mattingly found a sequence
//   in the simulator with amp-hours to spare.
//       "Powering up from cold, which has never been done"
//
// Ranking, for whoever prunes: 1202 first, with a searchable payload, then
// the retroreflectors, then rope memory. Test each against the binding rule
// by asking what a reader would type into a search box after the last line,
// and whether the answer hands back the first.
//
// EXTREMOPHILES, all fact-checked, none placed. There is no biology block in
// the list yet - Armillaria and Phorusrhacidae are the whole of it - so these
// want to go in together or not at all, and four in a row would be a theme
// rather than seasoning.
//   "Killing Picrophilus with water"        grows optimally at pH 0.7 and
//                                           lyses above pH 4 (Schleper et
//                                           al., J Bacteriol 177:7050)
//   "Measuring Thiomargarita in centimetres"  a single bacterial cell up to
//                                           2 cm, DNA in membrane-bound
//                                           organelles (Science, Jun 2022).
//                                           Mean is 9 mm; "up to" is load
//                                           bearing
//   "Descending with Desulforudis audaxviator"  2.8 km down in Mponeng, >99.9%
//                                           of its community, running on
//                                           radiolysis. Verne's Latin for
//                                           "descend, bold traveller", which
//                                           is why the verb echoes it
//   "Following the cable bacteria to the oxygen"  filaments of thousands of
//                                           cells conducting electrons
//                                           centimetres through sediment
//
// PHYSICS, checked, unplaced. The first is the best of them and the reason
// it is not in the list is that the payload is a number nobody can search:
//   Kochen-Specker. Nobody knows the smallest set of directions proving
//   quantum values cannot be pre-assigned. 31 known, 24 proven as a floor,
//   and the machine proof of the floor runs to over forty terabytes. The
//   size moves between paper revisions - 40.3 TiB, 41.6, 42.9 uncompressed -
//   so only "over forty" survives all of them.
//   Zel'dovich rotational superradiance: waves off a spinning absorber come
//   back amplified, confirmed acoustically in 2020 at 30% gain.
//   Unruh: one kelvin costs 2.47e20 m/s^2, and it has never been observed.
//
// AND A NOTE ON THE INSTRUMENT, since it cost real tokens. Two workflows
// generated 732 candidates between them. Nothing above came from their
// jokes - every line was rewritten from the underlying fact, because they
// were told sequences were preferred and that instruction was wrong. What
// they were actually good for was the fact-check: the DO NOT USE block above
// is the return on it, and it is worth more than the additions.
// ---------------------------------------------------------------------------
