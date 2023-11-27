#include <fcntl.h>
#include <getopt.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "tools-util.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/error.h"
#include "libbcachefs/journal_io.h"
#include "libbcachefs/journal_seq_blacklist.h"
#include "libbcachefs/super.h"

static const char *NORMAL	= "\x1B[0m";
static const char *RED		= "\x1B[31m";

static void list_journal_usage(void)
{
	puts("bcachefs list_journal - print contents of journal\n"
	     "Usage: bcachefs list_journal [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -a                                Read entire journal, not just dirty entries\n"
	     "  -n, --nr-entries=nr               Number of journal entries to print, starting from the most recent\n"
	     "  -t, --transaction-filter=bbpos    Filter transactions not updating <bbpos>\n"
	     "  -k, --key-filter=btree            Filter keys not updating btree\n"
	     "  -v, --verbose                     Verbose mode\n"
	     "  -h, --help                        Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static void star_start_of_lines(char *buf)
{
	char *p = buf;

	if (*p == ' ')
		*p = '*';

	while ((p = strstr(p, "\n ")))
		p[1] = '*';
}

static inline bool entry_is_transaction_start(struct jset_entry *entry)
{
	return entry->type == BCH_JSET_ENTRY_log && !entry->level;
}

typedef DARRAY(struct bbpos) d_bbpos;
typedef DARRAY(enum btree_id) d_btree_id;

static bool bkey_matches_filter(d_bbpos filter, struct jset_entry *entry, struct bkey_i *k)
{
	struct bbpos *i;

	darray_for_each(filter, i) {
		if (i->btree != entry->btree_id)
			continue;

		if (bkey_eq(i->pos, k->k.p))
			return true;

		if (btree_node_type_is_extents(i->btree) &&
		    bkey_ge(i->pos, bkey_start_pos(&k->k)) &&
		    bkey_lt(i->pos, k->k.p))
			return true;
	}
	return false;
}

static bool entry_matches_transaction_filter(struct jset_entry *entry,
					     d_bbpos filter)
{
	if (entry->type == BCH_JSET_ENTRY_btree_root ||
	    entry->type == BCH_JSET_ENTRY_btree_keys ||
	    entry->type == BCH_JSET_ENTRY_overwrite) {
		struct bkey_i *k;

		jset_entry_for_each_key(entry, k)
			if (bkey_matches_filter(filter, entry, k))
				return true;
	}

	return false;
}

static bool should_print_transaction(struct jset_entry *entry, struct jset_entry *end,
				     d_bbpos filter)
{
	if (!filter.nr)
		return true;

	for (entry = vstruct_next(entry);
	     entry != end && !entry_is_transaction_start(entry);
	     entry = vstruct_next(entry))
		if (entry_matches_transaction_filter(entry, filter))
			return true;

	return false;
}

static bool should_print_entry(struct jset_entry *entry, d_btree_id filter)
{
	struct bkey_i *k;
	enum btree_id *id;

	if (!filter.nr)
		return true;

	if (entry->type != BCH_JSET_ENTRY_btree_root &&
	    entry->type != BCH_JSET_ENTRY_btree_keys &&
	    entry->type != BCH_JSET_ENTRY_overwrite)
		return true;

	jset_entry_for_each_key(entry, k)
		darray_for_each(filter, id)
			if (entry->btree_id == *id)
				return true;

	return false;
}

static void journal_entries_print(struct bch_fs *c, unsigned nr_entries,
				  d_bbpos transaction_filter,
				  d_btree_id key_filter)
{
	struct journal_replay *p, **_p;
	struct genradix_iter iter;
	struct printbuf buf = PRINTBUF;

	genradix_for_each(&c->journal_entries, iter, _p) {
		p = *_p;
		if (!p)
			continue;

		if (le64_to_cpu(p->j.seq) + nr_entries < atomic64_read(&c->journal.seq))
			continue;

		bool blacklisted = p->ignore ||
			bch2_journal_seq_is_blacklisted(c,
					le64_to_cpu(p->j.seq), false);

		if (transaction_filter.nr) {
			if (blacklisted)
				printf("blacklisted ");

			printf("journal entry     %llu\n", le64_to_cpu(p->j.seq));

			printbuf_reset(&buf);

			prt_printf(&buf,
				   "  version         %u\n"
				   "  last seq        %llu\n"
				   "  flush           %u\n"
				   "  written at      ",
				   le32_to_cpu(p->j.version),
				   le64_to_cpu(p->j.last_seq),
				   !JSET_NO_FLUSH(&p->j));
			bch2_journal_ptrs_to_text(&buf, c, p);

			if (blacklisted)
				star_start_of_lines(buf.buf);
			printf("%s\n", buf.buf);
			printbuf_reset(&buf);
		}

		struct jset_entry *entry = p->j.start;
		struct jset_entry *end = vstruct_last(&p->j);
		while (entry != end) {

			/*
			 * log entries denote the start of a new transaction
			 * commit:
			 */
			if (entry_is_transaction_start(entry)) {
				if (!should_print_transaction(entry, end, transaction_filter)) {
					do {
						entry = vstruct_next(entry);
					} while (entry != end && !entry_is_transaction_start(entry));

					continue;
				}

				prt_newline(&buf);
			}

			if (!should_print_entry(entry, key_filter))
				goto next;

			bool highlight = entry_matches_transaction_filter(entry, transaction_filter);
			if (highlight)
				fputs(RED, stdout);

			printbuf_indent_add(&buf, 4);
			bch2_journal_entry_to_text(&buf, c, entry);

			if (blacklisted)
				star_start_of_lines(buf.buf);
			printf("%s\n", buf.buf);
			printbuf_reset(&buf);

			if (highlight)
				fputs(NORMAL, stdout);
next:
			entry = vstruct_next(entry);
		}
	}

	printbuf_exit(&buf);
}

int cmd_list_journal(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "nr-entries",		required_argument,	NULL, 'n' },
		{ "transaction-filter",	required_argument,	NULL, 't' },
		{ "key-filter",		required_argument,	NULL, 'k' },
		{ "verbose",		no_argument,		NULL, 'v' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_opts opts = bch2_opts_empty();
	u32 nr_entries = U32_MAX;
	d_bbpos		transaction_filter = { 0 };
	d_btree_id	key_filter = { 0 };
	int opt;

	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, degraded,		true);
	opt_set(opts, errors,		BCH_ON_ERROR_continue);
	opt_set(opts, fix_errors,	FSCK_FIX_yes);
	opt_set(opts, keep_journal,	true);
	opt_set(opts, read_journal_only,true);

	while ((opt = getopt_long(argc, argv, "an:t:k:vh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'a':
			opt_set(opts, read_entire_journal, true);
			break;
		case 'n':
			if (kstrtouint(optarg, 10, &nr_entries))
				die("error parsing nr_entries");
			opt_set(opts, read_entire_journal, true);
			break;
		case 't':
			darray_push(&transaction_filter, bbpos_parse(optarg));
			break;
		case 'k':
			darray_push(&key_filter, read_string_list_or_die(optarg, __bch2_btree_ids, "btree id"));
			break;
		case 'v':
			opt_set(opts, verbose, true);
			break;
		case 'h':
			list_journal_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc)
		die("Please supply device(s) to open");

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], bch2_err_str(PTR_ERR(c)));

	journal_entries_print(c, nr_entries, transaction_filter, key_filter);
	bch2_fs_stop(c);
	return 0;
}
