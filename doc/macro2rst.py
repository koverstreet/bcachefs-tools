#!/usr/bin/env python3
'''
A utility script for generating documentation.

Preprocessor macro output from opts_macro.h is parsed and combined with
bcachefs.5.rst.tmpl to generate bcachefs.5.rst.

>=python3.6
'''

import sys
import re

INDENT = '       '
TEMPLATE = './doc/bcachefs.5.rst.tmpl'
RST_FILE= './doc/bcachefs.5.rst'
SANITIZE_CHARS = [
            '\\\\n',
            '\\n',
            '  ',
            '"',
            '\\',
        ]

def sanitize(text):
    '''
    Parses opts_macro.h preprocessor output
    :param text: text to sanitize
    :type text: str
    :returns: a list of options
    :rtype: list
    '''

    args = []
    reg = re.search('FMT_START_SECTION(.*)FMT_END_SECTION', text,
            flags=re.DOTALL)
    if not reg:
        raise re.error('no text found')

    # decoding would probably be better, but this works
    for char in SANITIZE_CHARS:
        text = text.replace(char, '')

    text = re.split(r'FMT_END_LINE', text)

    # this seemed easier than getting preprocessor macros to play nice
    # with python's builtin csv module
    for line in text:
        vals = line.split(';')
        if not vals:
            continue
        if len(vals) != 4:
            continue
        vals = list(map(str.strip, vals))
        name, is_bool, desc, arg_name = vals

        # this macro value from opts.h indicates that no values are passed
        if is_bool == 'OPT_BOOL()':
            args.append(f'--{name}\n{INDENT}{desc}')
        else:
            args.append(f'--{name} <{arg_name}>\n{INDENT}{desc}')
    if not args:
        raise re.error('no args found, likely parsing error')

    return args


def main():
    ''' Transform stdin to option list and write templated output to new file '''
    out = ''

    stdin = sys.stdin.read()
    opts = sanitize(stdin)
    opts = '\n'.join(opts)

    # Insert into template
    with open(TEMPLATE, 'r') as in_handle:
        in_handle = in_handle.read()
    out = in_handle.replace('OPTIONS_TABLE', opts)
    with open(RST_FILE, 'w') as out_handle:
        out_handle.write(out)


if __name__ == '__main__':
    main()
