#include <ctype.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libbcachefs.h"
#include "tools-util.h"

#include "bcachefs.h"

/* option parsing */

#include <getopt.h>

void bch2_opt_strs_free(struct bch_opt_strs *opts)
{
	unsigned i;

	for (i = 0; i < bch2_opts_nr; i++) {
		free(opts->by_id[i]);
		opts->by_id[i] = NULL;
	}
}

static bool opt_type_filter(const struct bch_option *opt, unsigned opt_types)
{
	if (!(opt->flags & opt_types))
		return false;

	if ((opt_types & OPT_FORMAT) &&
	    !opt->set_sb && !opt->set_member)
		return false;

	return true;
}

const struct bch_option *bch2_cmdline_opt_parse(int argc, char *argv[],
						unsigned opt_types)
{
	if (optind >= argc)
		return NULL;

	if (argv[optind][0] != '-' ||
	    argv[optind][1] != '-')
		return NULL;

	char *optstr = strdup(argv[optind] + 2);
	optarg = NULL;

	char *eq = strchr(optstr, '=');
	if (eq) {
		*eq = '\0';
		optarg = eq + 1;
	}

	int optid = bch2_opt_lookup(optstr);
	if (optid < 0)
		goto noopt;

	const struct bch_option *opt = bch2_opt_table + optid;
	if (!opt_type_filter(opt, opt_types))
		goto noopt;

	optind++;

	if (!optarg) {
		if (opt->type != BCH_OPT_BOOL)
			optarg = argv[optind++];
		else
			optarg = "1";
	}

	return opt;
noopt:
	free(optstr);
	return NULL;
}

struct bch_opt_strs bch2_cmdline_opts_get(int *argc, char *argv[],
					  unsigned opt_types)
{
	struct bch_opt_strs opts;
	unsigned i = 1;

	memset(&opts, 0, sizeof(opts));

	while (i < *argc) {
		char *optstr = strcmp_prefix(argv[i], "--");
		char *valstr = NULL, *p;
		int optid, nr_args = 1;

		if (!optstr) {
			i++;
			continue;
		}

		optstr = strdup(optstr);

		p = optstr;
		while (isalpha(*p) || *p == '_')
			p++;

		if (*p == '=') {
			*p = '\0';
			valstr = p + 1;
		}

		optid = bch2_opt_lookup(optstr);
		if (optid < 0 ||
		    !(bch2_opt_table[optid].flags & opt_types)) {
			i++;
			goto next;
		}

		if (!valstr &&
		    bch2_opt_table[optid].type != BCH_OPT_BOOL) {
			nr_args = 2;
			valstr = argv[i + 1];
		}

		if (!valstr)
			valstr = "1";

		opts.by_id[optid] = strdup(valstr);

		*argc -= nr_args;
		memmove(&argv[i],
			&argv[i + nr_args],
			sizeof(char *) * (*argc - i));
		argv[*argc] = NULL;
next:
		free(optstr);
	}

	return opts;
}

struct bch_opts bch2_parse_opts(struct bch_opt_strs strs)
{
	struct bch_opts opts = bch2_opts_empty();
	struct printbuf err = PRINTBUF;
	unsigned i;
	int ret;
	u64 v;

	for (i = 0; i < bch2_opts_nr; i++) {
		if (!strs.by_id[i])
			continue;

		ret = bch2_opt_parse(NULL,
				     &bch2_opt_table[i],
				     strs.by_id[i], &v, &err);
		if (ret < 0 && ret != -BCH_ERR_option_needs_open_fs)
			die("Invalid option %s", err.buf);

		bch2_opt_set_by_id(&opts, i, v);
	}

	printbuf_exit(&err);
	return opts;
}

#define newline(c)		\
	do {			\
		printf("\n");	\
		c = 0;		\
	} while(0)
void bch2_opts_usage(unsigned opt_types)
{
	const struct bch_option *opt;
	unsigned i, c = 0, helpcol = 32;

	for (opt = bch2_opt_table;
	     opt < bch2_opt_table + bch2_opts_nr;
	     opt++) {
		if (!opt_type_filter(opt, opt_types))
			continue;

		c += printf("      --%s", opt->attr.name);

		switch (opt->type) {
		case BCH_OPT_BOOL:
			break;
		case BCH_OPT_STR:
			c += printf("=(");
			for (i = 0; opt->choices[i]; i++) {
				if (i)
					c += printf("|");
				c += printf("%s", opt->choices[i]);
			}
			c += printf(")");
			break;
		default:
			c += printf("=%s", opt->hint);
			break;
		}

		if (opt->help) {
			const char *l = opt->help;

			if (c > helpcol) {
				newline(c);
			}

			while (1) {
				const char *n = strchrnul(l, '\n');

				while (c < helpcol-1) {
					putchar(' ');
					c++;
				}
				printf("%.*s", (int) (n - l), l);
				newline(c);

				if (!*n)
					break;
				l = n + 1;
			}
		} else {
			newline(c);
		}
	}
}

