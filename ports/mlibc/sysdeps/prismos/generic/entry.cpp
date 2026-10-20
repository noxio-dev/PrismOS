#include <stdint.h>
#include <stdlib.h>
#include <mlibc/elf/startup.h>

/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The static-binary entry: crt1.S hands the System V initial stack (argc /
 * argv / envp / auxv, exactly what PrismOS's ELF loader builds) to
 * __mlibc_entry; __dlapi_enter parses it for a static build, then main.
 */

extern "C" void __dlapi_enter(uintptr_t *);

extern char **environ;

extern "C" void __mlibc_entry(uintptr_t *entry_stack, int (*main_fn)(int argc, char *argv[], char *env[])) {
	__dlapi_enter(entry_stack);

	auto result = main_fn(mlibc::entry_stack.argc, mlibc::entry_stack.argv, environ);
	exit(result);
}
