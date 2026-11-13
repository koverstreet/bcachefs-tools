/* The shim's C that Rust calls into: bios, sysfs, shrinkers and the page
 * size. */

#include <linux/bio.h>
#include <linux/page.h>
#include <linux/shrinker.h>
#include <linux/sysfs.h>
