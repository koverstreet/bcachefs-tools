// SPDX-License-Identifier: GPL-2.0
#include "bcachefs.h"

#include "alloc/types_gen.h"
#include "alloc/buckets.h"

#include "btree/cache.h"
#include "btree/iter.h"
#include "btree/key_cache.h"
#include "btree/locking.h"
#include "btree/interior.h"

#include "data/keylist.h"
#include "data/move_gen.h"

#include "util/six.h"

#include <linux/blktrace_api.h>

/* And rust_do_trace_<name>(), each tracepoint's, for sb/counters.rs */
#define CREATE_TRACE_POINTS
#define CREATE_RUST_TRACE_POINTS
#include "debug/trace.h"
