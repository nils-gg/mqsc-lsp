// External scanner for tree-sitter-mqsc
//
// Handles the three lexical features of MQSC that a context-free internal
// scanner cannot express:
//
//   * comment      - a line whose FIRST character is '*' is a comment; the
//                    token includes the line break that ends it.
//   * continuation - '-' or '+' as the last non-blank character on a line
//                    continues the command on the following line. Blank lines
//                    after a continuation are skipped (MQSC resumes at the
//                    next non-blank, non-comment line). Exception INSIDE a
//                    string: a blank line there is ambiguous (leading blanks
//                    after '-' are literal content), so the string is ended
//                    as unterminated at that point. Comment lines are
//                    left for the parser, which accepts them as extras.
//   * string       - '...' with '' as an embedded literal quote. Strings may
//                    be split across lines with '-' or '+' at the end of a
//                    line: '-' continues from the start of the next line
//                    (leading blanks are significant), '+' continues from the
//                    first non-blank character of the next line. A string
//                    that hits end-of-line without a continuation is
//                    reported as a (unterminated) string token ending before
//                    the newline so the language server can flag it; a string
//                    that hits EOF becomes a token spanning to EOF.
//
// Whitespace skipping: spaces, tabs and carriage returns are skipped, but a
// bare newline is NEVER skipped - the grammar needs to see it as a token.
// Consequence: a pure-CR (classic Mac) file, where every line break is '\r',
// lexes as one giant line; accepted since real MQSC files are LF or CRLF.

#include "tree_sitter/parser.h"

enum TokenType {
  COMMENT_TOKEN,
  CONTINUATION_TOKEN,
  STRING_TOKEN,
};

void *tree_sitter_mqsc_external_scanner_create(void) { return NULL; }

void tree_sitter_mqsc_external_scanner_destroy(void *payload) {}

unsigned tree_sitter_mqsc_external_scanner_serialize(void *payload,
                                                     char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_mqsc_external_scanner_deserialize(void *payload,
                                                   const char *buffer,
                                                   unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

// Consume to end of line, then the line break itself (if present).
static void consume_to_eol(TSLexer *lexer) {
  while (!lexer->eof(lexer) && lexer->lookahead != '\n') {
    lexer->advance(lexer, false);
  }
  if (lexer->lookahead == '\n') {
    lexer->advance(lexer, false);
  }
}

// After a continuation's line break, consume any number of blank lines
// (spaces/tabs/CRs followed by a newline). Stops at the first non-blank
// character (including a comment '*') or EOF.
static void skip_blank_lines(TSLexer *lexer) {
  for (;;) {
    while (!lexer->eof(lexer) && lexer->lookahead != '\n' &&
           lexer->lookahead != ' ' && lexer->lookahead != '\t' &&
           lexer->lookahead != '\r') {
      return; // non-blank character reached
    }
    if (lexer->eof(lexer)) {
      return;
    }
    lexer->advance(lexer, false); // whitespace or newline
  }
}

bool tree_sitter_mqsc_external_scanner_scan(void *payload, TSLexer *lexer,
                                           const bool *valid_symbols) {
  (void)payload;

  // The external scanner may be invoked before the internal lexer has skipped
  // blanks; do the skipping ourselves (newlines are never skipped - they are
  // tokens). This must not disturb the comment column check below because
  // get_column() counts everything consumed so far on the line.
  while (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
         lexer->lookahead == '\r') {
    lexer->advance(lexer, true);
  }

  // --- comment: '*' in the very first column -------------------------------
  if (valid_symbols[COMMENT_TOKEN] && lexer->lookahead == '*' &&
      lexer->get_column(lexer) == 0) {
    lexer->advance(lexer, false); // the '*'
    consume_to_eol(lexer);
    lexer->mark_end(lexer);
    lexer->result_symbol = COMMENT_TOKEN;
    return true;
  }

  // --- line continuation: '+' or '-' at end of line ------------------------
  if (valid_symbols[CONTINUATION_TOKEN] &&
      (lexer->lookahead == '-' || lexer->lookahead == '+')) {
    lexer->advance(lexer, false); // the marker
    while (!lexer->eof(lexer) && lexer->lookahead != '\n' &&
           (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
            lexer->lookahead == '\r')) {
      lexer->advance(lexer, false);
    }
    if (lexer->lookahead != '\n') {
      return false; // marker is not at the end of a line
    }
    lexer->advance(lexer, false); // the newline
    skip_blank_lines(lexer);
    lexer->mark_end(lexer);
    lexer->result_symbol = CONTINUATION_TOKEN;
    return true;
  }

  // --- quoted string --------------------------------------------------------
  if (valid_symbols[STRING_TOKEN] && lexer->lookahead == '\'') {
    lexer->advance(lexer, false); // opening quote
    for (;;) {
      if (lexer->eof(lexer)) {
        // Unterminated at EOF: emit the token up to EOF; the server flags it.
        lexer->mark_end(lexer);
        lexer->result_symbol = STRING_TOKEN;
        return true;
      }
      if (lexer->lookahead == '\'') {
        lexer->advance(lexer, false); // closing quote
        if (lexer->lookahead == '\'') {
          lexer->advance(lexer, false); // '' -> one literal quote
          continue;
        }
        lexer->mark_end(lexer);
        lexer->result_symbol = STRING_TOKEN;
        return true;
      }
      if (lexer->lookahead == '\n') {
        // No continuation before the newline: the string is unterminated.
        // End the token before the newline so the parser can recover and the
        // server can report it.
        lexer->mark_end(lexer);
        lexer->result_symbol = STRING_TOKEN;
        return true;
      }
      if (lexer->lookahead == '-' || lexer->lookahead == '+') {
        // Possibly an in-string continuation; remember position via mark_end
        // (not advanced yet) so the token can end before the marker when the
        // probe fails.
        bool plus = lexer->lookahead == '+';
        lexer->mark_end(lexer);
        lexer->advance(lexer, false); // the marker
        while (!lexer->eof(lexer) && lexer->lookahead != '\n' &&
               (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
                lexer->lookahead == '\r')) {
          lexer->advance(lexer, false);
        }
        if (lexer->lookahead != '\n') {
          // Not a continuation: the marker and the blanks we consumed are
          // ordinary string content; mark_end was left before them, so
          // continue scanning content from where we are.
          continue;
        }
        lexer->advance(lexer, false); // consume the newline
        if (plus) {
          // '+' continues at the first non-blank of the next line.
          while (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
                 lexer->lookahead == '\r') {
            lexer->advance(lexer, false);
          }
        }
        // '-' continues at the very first column: any leading blanks of the
        // next line become literal string content (handled by this loop's
        // default case on the next iteration).
        continue;
      }
      lexer->advance(lexer, false); // ordinary content character
    }
  }

  return false;
}
