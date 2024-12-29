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
	     "                                    Or entries not matching the range <bbpos-bbpos>\n"
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

static inline bool entry_is_log_msg(struct jset_entry *entry)
{
	return entry->type == BCH_JSET_ENTRY_log && entry->level;
}

typedef DARRAY(struct bbpos_range) d_bbpos_range;
typedef DARRAY(enum btree_id) d_btree_id;

static bool bkey_matches_filter(d_bbpos_range filter, struct jset_entry *entry, struct bkey_i *k)
{
	darray_for_each(filter, i) {
		struct bbpos k_start	= BBPOS(entry->btree_id, bkey_start_pos(&k->k));
		struct bbpos k_end	= BBPOS(entry->btree_id, k->k.p);

		if (bbpos_cmp(k_start, i->start) >= 0 &&
		    bbpos_cmp(k_end, i->end) <= 0)
			return true;
	}
	return false;
}

static bool entry_matches_transaction_filter(struct jset_entry *entry,
					     d_bbpos_range filter)
{
	if (entry->type == BCH_JSET_ENTRY_btree_root ||
	    entry->type == BCH_JSET_ENTRY_btree_keys ||
	    entry->type == BCH_JSET_ENTRY_overwrite)
		jset_entry_for_each_key(entry, k)
			if (bkey_matches_filter(filter, entry, k))
				return true;
	return false;
}

static bool should_print_transaction(struct jset_entry *entry, struct jset_entry *end,
				     darray_str msg_filter,
				     d_bbpos_range key_filter)
{
	struct jset_entry_log *l = container_of(entry, struct jset_entry_log, entry);
	unsigned b = jset_entry_log_msg_bytes(l);

	darray_for_each(msg_filter, i)
		if (!strncmp(*i, l->d, b))
			return false;

	if (!key_filter.nr)
		return true;

	for (entry = vstruct_next(entry);
	     entry != end && !entry_is_transaction_start(entry);
	     entry = vstruct_next(entry))
		if (entry_is_log_msg(entry) ||
		    entry_matches_transaction_filter(entry, key_filter))
			return true;

	return false;
}

static bool should_print_entry(struct jset_entry *entry, d_btree_id filter)
{
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

static void journal_entry_header_to_text(struct printbuf *out,
					 struct bch_fs *c,
					 struct journal_replay *p, bool blacklisted)
{
	if (blacklisted)
		prt_str(out, "blacklisted ");

	prt_printf(out,
		   "journal entry     %llu\n"
		   "  version         %u\n"
		   "  last seq        %llu\n"
		   "  flush           %u\n"
		   "  written at      ",
		   le64_to_cpu(p->j.seq),
		   le32_to_cpu(p->j.version),
		   le64_to_cpu(p->j.last_seq),
		   !JSET_NO_FLUSH(&p->j));
	bch2_journal_ptrs_to_text(out, c, p);

	if (blacklisted)
		star_start_of_lines(out->buf);
}

static void journal_entry_header_print(struct bch_fs *c, struct journal_replay *p, bool blacklisted)
{
	struct printbuf buf = PRINTBUF;
	journal_entry_header_to_text(&buf, c, p, blacklisted);
	printf("%s\n", buf.buf);
	printbuf_exit(&buf);
}

static void journal_entries_print(struct bch_fs *c, unsigned nr_entries,
				  darray_str transaction_msg_filter,
				  d_bbpos_range transaction_key_filter,
				  d_btree_id key_filter)
{
	struct journal_replay *p, **_p;
	struct genradix_iter iter;
	struct printbuf buf = PRINTBUF;

	genradix_for_each(&c->journal_entries, iter, _p) {
		bool printed_header = false;

		p = *_p;
		if (!p)
			continue;

		if (le64_to_cpu(p->j.seq) + nr_entries < atomic64_read(&c->journal.seq))
			continue;

		bool blacklisted = p->ignore_blacklisted ||
			bch2_journal_seq_is_blacklisted(c,
					le64_to_cpu(p->j.seq), false);

		if (!transaction_msg_filter.nr &&
		    !transaction_key_filter.nr) {
			journal_entry_header_print(c, p, blacklisted);
			printed_header = true;
		}

		struct jset_entry *entry = p->j.start;
		struct jset_entry *end = vstruct_last(&p->j);
		while (entry != end) {

			/*
			 * log entries denote the start of a new transaction
			 * commit:
			 */
			if (entry_is_transaction_start(entry)) {
				if (!should_print_transaction(entry, end,
							      transaction_msg_filter,
							      transaction_key_filter)) {
					do {
						entry = vstruct_next(entry);
					} while (entry != end && !entry_is_transaction_start(entry));

					continue;
				}

				prt_newline(&buf);
			}

			if (!should_print_entry(entry, key_filter))
				goto next;

			if (!printed_header)
				journal_entry_header_print(c, p, blacklisted);
			printed_header = true;

			bool highlight = entry_matches_transaction_filter(entry, transaction_key_filter);
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
	darray_str	transaction_msg_filter = {};
	d_bbpos_range	transaction_key_filter = {};
	d_btree_id	key_filter = {};
	int opt;

	opt_set(opts, noexcl,		true);
	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, read_only,	true);
	opt_set(opts, degraded,		true);
	opt_set(opts, very_degraded,		true);
	opt_set(opts, errors,		BCH_ON_ERROR_continue);
	opt_set(opts, fix_errors,	FSCK_FIX_yes);
	opt_set(opts, retain_recovery_info ,true);
	opt_set(opts, read_journal_only,true);

	while ((opt = getopt_long(argc, argv, "an:m:t:k:vh",
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
		case 'm':
			darray_push(&transaction_msg_filter, strdup(optarg));
			break;
		case 't':
			darray_push(&transaction_key_filter, bbpos_range_parse(optarg));
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

	darray_str devs = get_or_split_cmdline_devs(argc, argv);

	struct bch_fs *c = bch2_fs_open(devs.data, devs.nr, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], bch2_err_str(PTR_ERR(c)));

	journal_entries_print(c, nr_entries,
			      transaction_msg_filter,
			      transaction_key_filter,
			      key_filter);
	bch2_fs_stop(c);
	return 0;
}
