#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "qcow2.h"
#include "tools-util.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/error.h"
#include "libbcachefs/journal_io.h"
#include "libbcachefs/journal_seq_blacklist.h"
#include "libbcachefs/super.h"

static void list_journal_usage(void)
{
	puts("bcachefs list_journal - print contents of journal\n"
	     "Usage: bcachefs list_journal [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -a            Read entire journal, not just dirty entries\n"
	     "  -n            Number of journal entries to print, starting from the most recent\n"
	     "  -v            Verbose mode\n"
	     "  -h            Display this help and exit\n"
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

int cmd_list_journal(int argc, char *argv[])
{
	struct bch_opts opts = bch2_opts_empty();
	u32 nr_entries = U32_MAX;
	int opt;

	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, degraded,		true);
	opt_set(opts, errors,		BCH_ON_ERROR_continue);
	opt_set(opts, fix_errors,	FSCK_OPT_YES);
	opt_set(opts, keep_journal,	true);
	opt_set(opts, read_journal_only,true);

	while ((opt = getopt(argc, argv, "an:vh")) != -1)
		switch (opt) {
		case 'a':
			opt_set(opts, read_entire_journal, true);
			break;
		case 'n':
			nr_entries = kstrtouint(optarg, 10, &nr_entries);
			opt_set(opts, read_entire_journal, true);
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
		die("error opening %s: %s", argv[0], strerror(-PTR_ERR(c)));

	struct journal_replay *p, **_p;
	struct genradix_iter iter;
	struct jset_entry *entry;
	struct printbuf buf = PRINTBUF;

	genradix_for_each(&c->journal_entries, iter, _p) {
		p = *_p;
		if (!p)
			continue;

		if (le64_to_cpu(p->j.seq) + nr_entries < atomic64_read(&c->journal.seq))
			continue;

		bool blacklisted =
			bch2_journal_seq_is_blacklisted(c,
					le64_to_cpu(p->j.seq), false);

		if (blacklisted)
			printf("blacklisted ");

		printf("journal entry       %llu\n", le64_to_cpu(p->j.seq));

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

		vstruct_for_each(&p->j, entry) {
			printbuf_reset(&buf);

			/*
			 * log entries denote the start of a new transaction
			 * commit:
			 */
			if (entry->type == BCH_JSET_ENTRY_log && !entry->level)
				prt_newline(&buf);
			printbuf_indent_add(&buf, 4);
			bch2_journal_entry_to_text(&buf, c, entry);

			if (blacklisted)
				star_start_of_lines(buf.buf);
			printf("%s\n", buf.buf);
		}
	}

	printbuf_exit(&buf);
	bch2_fs_stop(c);
	return 0;
}

static void kill_btree_node_usage(void)
{
	puts("bcachefs kill_btree_node - make btree nodes unreadable\n"
	     "Usage: bcachefs kill_btree_node [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -b (extents|inodes|dirents|xattrs)    Btree to delete from\n"
	     "  -l level                              Level to delete from (0 == leaves)\n"
	     "  -i index                              Index of btree node to kill\n"
	     "  -h                                    Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_kill_btree_node(int argc, char *argv[])
{
	struct bch_opts opts = bch2_opts_empty();
	enum btree_id btree_id = 0;
	unsigned level = 0;
	u64 node_index = 0;
	int opt;

	opt_set(opts, read_only,	true);

	while ((opt = getopt(argc, argv, "b:l:i:h")) != -1)
		switch (opt) {
		case 'b':
			btree_id = read_string_list_or_die(optarg,
						bch2_btree_ids, "btree id");
			break;
		case 'l':
			if (kstrtouint(optarg, 10, &level) || level >= BTREE_MAX_DEPTH)
				die("invalid level");
			break;
		case 'i':
			if (kstrtoull(optarg, 10, &node_index))
				die("invalid index %s", optarg);
			break;
		case 'h':
			kill_btree_node_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc)
		die("Please supply device(s)");

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], strerror(-PTR_ERR(c)));

	struct btree_trans trans;
	struct btree_iter iter;
	struct btree *b;
	int ret;
	void *zeroes;

	ret = posix_memalign(&zeroes, c->opts.block_size, c->opts.block_size);
	if (ret)
		die("error %s from posix_memalign", strerror(ret));

	bch2_trans_init(&trans, c, 0, 0);

	__for_each_btree_node(&trans, iter, btree_id, POS_MIN, 0, level, 0, b, ret) {
		if (b->c.level != level)
			continue;

		if (!node_index) {
			struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(bkey_i_to_s_c(&b->key));
			const struct bch_extent_ptr *ptr;

			struct printbuf buf = PRINTBUF;

			bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));
			bch_info(c, "killing btree node %s", buf.buf);
			printbuf_exit(&buf);

			bkey_for_each_ptr(ptrs, ptr) {
				struct bch_dev *ca = bch_dev_bkey_exists(c, ptr->dev);

				ret = pwrite(ca->disk_sb.bdev->bd_fd, zeroes,
					     c->opts.block_size, ptr->offset << 9);
				if (ret != c->opts.block_size) {
					bch_err(c, "pwrite error: expected %u got %i %s",
						c->opts.block_size, ret, strerror(errno));
					ret = EXIT_FAILURE;
					goto done;
				}
			}
			goto done;
		}

		node_index--;
	}
	if (ret)
		bch_err(c, "error %i walking btree nodes", ret);
	else
		bch_err(c, "node at specified index not found");
	ret = EXIT_FAILURE;
done:
	bch2_trans_iter_exit(&trans, &iter);
	bch2_trans_exit(&trans);

	bch2_fs_stop(c);
	return ret;
}
