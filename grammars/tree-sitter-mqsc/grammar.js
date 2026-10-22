/**
 * tree-sitter-mqsc — grammar for IBM MQ Script (MQSC) commands.
 *
 * Language rules (MQ SC1024 "Script (MQSC) Command Reference", §1.1):
 *  - Blanks and commas are separators.
 *  - '...' delimits strings; '' inside a string is one literal quote.
 *  - A line whose first character is '*' is a comment; blank lines are ignored.
 *  - A line ending with '-' or '+' continues the command on the next line.
 *  - ';' terminates a command; commands otherwise end at the line break.
 *  - '(' value ... ')' encloses parameter values; '=' is accepted on z/OS
 *    instead of the parentheses.
 *  - '*' and '?' are wildcards; ':' forms inclusive ranges (TRACE).
 *
 * Design: the grammar captures *structure*, not semantics. A command is a
 * verb word followed by a flat list of words and parenthesised argument
 * groups. Which word is the object type, which words are keyword parameters
 * and which groups belong to which parameter is context-sensitive (it
 * depends on the verb/object-type) and therefore resolved by the language
 * server against the MQSC model (mqsc-model.json). Keeping the grammar small
 * and deterministic is what makes the parse tree resilient to the many
 * slightly-wrong-but-parseable commands found in real MQSC scripts.
 *
 * External scanner handles: '*' column-0 comments (swallowing their line
 * break), '...' strings with '' escapes and in-string continuations, and
 * '+'/'-' end-of-line continuations (the continuation token absorbs its line
 * break and any blank lines that follow, per §1.1.3).
 */

'use strict';

module.exports = grammar({
  name: 'mqsc',

  extras: ($) => [/[ \t\r]+/, $.comment],

  externals: ($) => [$.comment, $._continuation, $.string],

  rules: {
    // A MQSC script is a sequence of commands, comments and blank lines.
    program: $ => repeat(choice($.command, $._line_break, $._semicolon)),

    // prec.right resolves the shift/reduce question "is the next word part
    // of this command or a new command?" in favour of *this command*: two MQSC
    // commands on the same line must be separated by ';' anyway, otherwise the
    // whole line is (correctly) treated as one malformed command that the
    // server flags.
    command: $ =>
      prec.right(
        seq(
          field('verb', alias($.word, $.verb)),
          repeat(
            choice(
              // z/OS console / CSQUTIL form: NAME=value, the value running to
              // a blank or comma separator.
              seq(optional($._gap), $.word, '=', optional($._value_list)),
              seq(optional($._gap), $.word),
              seq(optional($._gap), $.argument),
              // bare wildcard as a "parameter": DISPLAY QLOCAL (*) *
              seq(optional($._gap), $.wildcard)
            )
          ),
          optional(choice($._line_break, $._semicolon))
        )
      ),

    // A parenthesised argument group: (value, value...). The first group of
    // a command usually carries the object name; later groups carry parameter
    // values and pair positionally with the word that precedes them.
    argument: $ => seq('(', optional($._value_list), ')'),

    // Value lists tolerate repeated commas ("multiple commas are equivalent
    // to a single comma") and line continuations after commas/values.
    // After the head, everything must be comma-led, so "DESCR(A B)" stays a
    // syntax error like real MQSC.
    _value_list: $ =>
      seq(optional($._gap), $._value_head, repeat($._value_element)),

    _value_head: $ => choice($._value_item, $._value_element),

    // prec.right: a continuation after a value attaches to this value item
    // rather than starting a new element (same text either way).
    _value_item: $ => prec.right(seq($.value, optional($._gap))),

    // prec.right resolves the "one element with a comma run vs several
    // elements" ambiguity (same text, two trees) in favour of the greedy form.
    _value_element: $ =>
      prec.right(seq(repeat1(','), optional($._gap), optional($._value_item))),

    // prec.right: a wildcard directly after a word extends that word's value
    // (PAY* is one generic value, not a value followed by a separate '*'),
    // and a number after ':' extends the range.
    value: $ =>
      prec.right(
        choice(
          // A name, optionally with wildcards: PAY*, sport/running/#, ABC?
          seq($.word, repeat($.wildcard)),
          // Inclusive range (TRACE only): 1:5 or 1:
          seq($.integer, ':', optional($.integer)),
          $.wildcard,
          $.word,
          $.integer,
          $.string
        )
      ),

    wildcard: _ => /[*?]/,

    integer: _ => /-?[0-9]+/,

    // A word token (verb, name, keyword...). Words may start with a digit
    // only when followed by another name character, so that plain numbers lex
    // as `integer`. A word may never start with '-' or '+'; those are handled
    // as line continuations (at EOL) or produce a lexical error.
    word: _ => /[A-Za-z_#$@%/.][-A-Za-z0-9_#$@%/.]*|[0-9]+[A-Za-z_#$@%/.][-A-Za-z0-9_#$@%/.]*/,

    // A "gap" inside a command: a line continuation token from the external
    // scanner. Its newline (and any blank lines) were consumed by the scanner,
    // so a raw newline never appears inside a command.
    _gap: $ => $._continuation,

    // Internal token: a bare newline. The external scanner's skip() never
    // skips it, so it is visible wherever the grammar allows blank lines or
    // command ends.
    _line_break: _ => /\n/,

    _semicolon: _ => ';',
  },
});
