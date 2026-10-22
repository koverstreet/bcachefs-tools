// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::c_opaque;

// util/thread_with_file.rs's StdioRedirect, to C: what c->stdio points to
c_opaque!(stdio_redirect);
