#include <stdio.h>
#include <stdlib.h>
#include <termios.h>
#include <unistd.h>

#include "crypto.h"

char *read_passphrase(const char *prompt)
{
	char *buf = NULL;
	size_t buflen = 0;
	ssize_t len;

	if (isatty(STDIN_FILENO)) {
		struct termios old, new;

		fprintf(stderr, "%s", prompt);
		fflush(stderr);

		if (tcgetattr(STDIN_FILENO, &old))
			die("error getting terminal attrs");

		/*
		 * This may be an early-boot console (initramfs) that no shell
		 * has ever configured, so ask for line input rather than
		 * inheriting it: without ICANON the tty does no erase
		 * processing and backspace lands in the passphrase as \177,
		 * and without ICRNL enter arrives as '\r', which getline()
		 * doesn't terminate on.
		 */
		new = old;
		new.c_lflag &= ~ECHO;
		new.c_lflag |= ICANON;
		new.c_iflag |= ICRNL;
		if (tcsetattr(STDIN_FILENO, TCSAFLUSH, &new))
			die("error setting terminal attrs");

		len = getline(&buf, &buflen, stdin);

		tcsetattr(STDIN_FILENO, TCSAFLUSH, &old);
		fprintf(stderr, "\n");
	} else {
		len = getline(&buf, &buflen, stdin);
	}

	if (len < 0)
		die("error reading passphrase");
	if (len && buf[len - 1] == '\n')
		buf[len - 1] = '\0';

	return buf;
}
