/*
 * Force-included into every HAL file (-include): what the HAL takes from Linux and
 * newlib lacks, and printf/fprintf going to the Rust log instead of stdout.
 */
#ifndef PC_COMPAT_H
#define PC_COMPAT_H

#include <stdio.h>
#include <sys/time.h>
#include <sys/types.h>
#include <time.h>

#ifndef CLOCK_MONOTONIC
#define CLOCK_MONOTONIC ((clockid_t)4)
#endif

int clock_nanosleep(clockid_t clock_id, int flags, const struct timespec *request, struct timespec *remain);

int pc_printf(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
int pc_fprintf(FILE *stream, const char *fmt, ...) __attribute__((format(printf, 2, 3)));

#define printf pc_printf
#define fprintf pc_fprintf

#endif
