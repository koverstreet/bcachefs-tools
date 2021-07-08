#include "../libbcachefs/opts.h"

/**
 * generate tables from definitions in opt.h
 */

#define NULL (null)

FMT_START_SECTION
#define x(_name, _shortopt, _type, _in_mem_type, _mode, _sb_opt, _desc , _usage)\
_name;_in_mem_type;_usage;_desc FMT_END_LINE
BCH_OPTS() FMT_END_SECTION
