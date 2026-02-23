#ifndef _BCACHEFS_TOOLS_RUST_TO_C_H
#define _BCACHEFS_TOOLS_RUST_TO_C_H

char *bch2_scan_devices(char *);

/* src/http.rs: */
void start_http(const char *listen);

#endif /* _BCACHEFS_TOOLS_RUST_TO_C_H */
