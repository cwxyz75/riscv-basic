#ifndef _OS_ERROR_H
#define _OS_ERROR_H
/* `error` prints `program: message[: strerror(errnum)]` on standard error and ends the program when `status` is not 0. */
extern unsigned int error_message_count;
void error(int status, int errnum, const char *format, ...);
void error_at_line(int status, int errnum, const char *filename, unsigned int linenum, const char *format, ...);
#endif
