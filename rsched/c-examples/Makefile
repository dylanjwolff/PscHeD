LIB_DIR  := ../target/debug
LIB      := $(LIB_DIR)/librsched.a
PRELOAD  := $(LIB_DIR)/librsched_preload.so
INC      := ../include
SAN_TIMEOUT ?= timeout 30

# uniform.c / uniform-lock.c / uaf.c have no main: they are compiled by
# build.rs into libcexamples.a for the Rust integration tests.  They do not
# appear as Makefile targets.

# TSAN targets link dynamically against the preload cdylib (built with
# --features tsan).  The cdylib exports the real pthread names so TSAN sees
# our scheduler as opaque pthread primitives.  rsched's own internal calls
# bypass the PLT via dlopen/dlsym-cached libpthread symbols, so neither the
# preload shim nor TSAN's interceptors are re-entered.
TSAN_CC      := clang
TSAN_CFLAGS  := -fsanitize=thread -g -Wall -Wextra
TSAN_LDFLAGS := -L$(LIB_DIR) -lrsched_preload \
                -Wl,-rpath,$(abspath $(LIB_DIR))

TSAN_TARGETS       := tsan_race tsan_no_race
ASAN_TARGETS       := asan_uaf asan_no_uaf
ASAN_PRE_TARGETS   := asan_uaf_pre asan_no_uaf_pre
UBSAN_TARGETS      := ubsan_ub ubsan_no_ub
UBSAN_PRE_TARGETS  := ubsan_ub_pre ubsan_no_ub_pre

# ASAN static targets: librsched.a (--features asan) + rsched.h macros.
# rsched.h redirects pthread_* → rsched_pthread_* at compile time.
ASAN_CC      := clang
ASAN_CFLAGS  := -fsanitize=address -g -Wall -Wextra -DRSCHED -I$(INC)
ASAN_LDFLAGS := -L$(LIB_DIR) -Wl,--start-group $(LIB) -Wl,--end-group \
                -lpthread -ldl -lm

# ASAN preload targets: librsched_preload.so + dynamic linking.
# No rsched.h macros; the preload intercepts the real pthread names at runtime.
# No --features asan needed: the preload always uses rpt().create (dlsym-cached
# real pthread_create) so ASAN's PLT interceptor is never involved.  ASAN heap
# UAF detection works via shadow memory and compile-time instrumentation.
ASAN_PRE_CC      := clang
ASAN_PRE_CFLAGS  := -fsanitize=address -g -Wall -Wextra
ASAN_PRE_LDFLAGS := -L$(LIB_DIR) -lrsched_preload \
                    -Wl,-rpath,$(abspath $(LIB_DIR))

# UBSan static targets: librsched.a + rsched.h macros.
UBSAN_CC      := clang
UBSAN_CFLAGS  := -fsanitize=undefined -g -Wall -Wextra -DRSCHED -I$(INC)
UBSAN_LDFLAGS := -L$(LIB_DIR) -Wl,--start-group $(LIB) -Wl,--end-group \
                 -lpthread -ldl -lm

# UBSan preload targets: librsched_preload.so + dynamic linking.
# No rsched.h macros; the preload intercepts pthread calls at runtime.
# UBSan instruments the user binary at compile time; no special lib features needed.
UBSAN_PRE_CC      := clang
UBSAN_PRE_CFLAGS  := -fsanitize=undefined -g -Wall -Wextra
UBSAN_PRE_LDFLAGS := -L$(LIB_DIR) -lrsched_preload \
                     -Wl,-rpath,$(abspath $(LIB_DIR))

.PHONY: all clean lib tsan tsan-lib check-tsan asan asan-lib check-asan \
        asan-pre asan-pre-lib check-asan-pre \
        ubsan ubsan-lib check-ubsan ubsan-pre ubsan-pre-lib check-ubsan-pre

all: lib

lib:
	cd .. && cargo build

tsan: tsan-lib $(TSAN_TARGETS)

# Build the preload cdylib with the tsan feature so __tsan_acquire/__tsan_release
# calls are compiled in.  Does not affect the regular lib target.
tsan-lib:
	cd .. && cargo build -p rsched-preload --features tsan

# Pattern rule: tsan_race.c → tsan_race, tsan_no_race.c → tsan_no_race
$(TSAN_TARGETS): %: %.c $(PRELOAD)
	$(TSAN_CC) $(TSAN_CFLAGS) -o $@ $< $(TSAN_LDFLAGS)

# Verify: no-race program exits cleanly; racy program triggers TSAN.
# setarch -R disables ASLR per-process: kernel 6.x places DSOs in address
# ranges that TSAN has reserved for its shadow memory, causing "unexpected
# memory mapping" at startup without the flag.
check-tsan: tsan
	@echo "=== tsan_no_race: expect clean exit ==="
	setarch x86_64 -R $(SAN_TIMEOUT) ./tsan_no_race
	@echo "=== tsan_race: expect data race report ==="
	@set +e; \
	setarch x86_64 -R env TSAN_OPTIONS="halt_on_error=1" $(SAN_TIMEOUT) ./tsan_race; \
	status=$$?; \
	if [ $$status -eq 124 ]; then echo "tsan_race timed out"; exit 1; fi; \
	if [ $$status -eq 0 ]; then echo "tsan_race unexpectedly exited cleanly"; exit 1; fi
	@echo "=== TSAN checks passed ==="

asan: asan-lib $(ASAN_TARGETS)

# Build librsched.a with --features asan so rsched calls pthread_create via the
# PLT, letting ASAN's interceptor register each thread in its thread registry.
asan-lib:
	cd .. && cargo build --features asan

$(ASAN_TARGETS): %: %.c $(LIB)
	$(ASAN_CC) $(ASAN_CFLAGS) -o $@ $< $(ASAN_LDFLAGS)

# Verify: no-uaf program exits cleanly; uaf program triggers ASAN.
# RANDOM_SEED=1 selects the interleaving where producer frees before consumer
# dereferences; most other seeds also trigger the UAF.
check-asan: asan
	@echo "=== asan_no_uaf: expect clean exit ==="
	$(SAN_TIMEOUT) ./asan_no_uaf
	@echo "=== asan_uaf: expect heap-use-after-free report ==="
	@set +e; \
	env ASAN_OPTIONS="halt_on_error=1" RANDOM_SEED=1 $(SAN_TIMEOUT) ./asan_uaf; \
	status=$$?; \
	if [ $$status -eq 124 ]; then echo "asan_uaf timed out"; exit 1; fi; \
	if [ $$status -eq 0 ]; then echo "asan_uaf unexpectedly exited cleanly"; exit 1; fi
	@echo "=== ASAN checks passed ==="

asan-pre: asan-pre-lib $(ASAN_PRE_TARGETS)

asan-pre-lib:
	cd .. && cargo build -p rsched-preload

# Pattern rule: asan_uaf.c → asan_uaf_pre, asan_no_uaf.c → asan_no_uaf_pre
$(ASAN_PRE_TARGETS): %_pre: %.c $(PRELOAD)
	$(ASAN_PRE_CC) $(ASAN_PRE_CFLAGS) -o $@ $< $(ASAN_PRE_LDFLAGS)

check-asan-pre: asan-pre
	@echo "=== asan_no_uaf_pre: expect clean exit ==="
	$(SAN_TIMEOUT) ./asan_no_uaf_pre
	@echo "=== asan_uaf_pre: expect heap-use-after-free report ==="
	@set +e; \
	env ASAN_OPTIONS="halt_on_error=1" RANDOM_SEED=1 $(SAN_TIMEOUT) ./asan_uaf_pre; \
	status=$$?; \
	if [ $$status -eq 124 ]; then echo "asan_uaf_pre timed out"; exit 1; fi; \
	if [ $$status -eq 0 ]; then echo "asan_uaf_pre unexpectedly exited cleanly"; exit 1; fi
	@echo "=== ASAN preload checks passed ==="

ubsan: ubsan-lib $(UBSAN_TARGETS)

ubsan-lib:
	cd .. && cargo build

$(UBSAN_TARGETS): %: %.c $(LIB)
	$(UBSAN_CC) $(UBSAN_CFLAGS) -o $@ $< $(UBSAN_LDFLAGS)

check-ubsan: ubsan
	@echo "=== ubsan_no_ub: expect clean exit ==="
	$(SAN_TIMEOUT) ./ubsan_no_ub
	@echo "=== ubsan_ub: expect signed-integer-overflow report ==="
	@set +e; \
	env UBSAN_OPTIONS="halt_on_error=1" $(SAN_TIMEOUT) ./ubsan_ub; \
	status=$$?; \
	if [ $$status -eq 124 ]; then echo "ubsan_ub timed out"; exit 1; fi; \
	if [ $$status -eq 0 ]; then echo "ubsan_ub unexpectedly exited cleanly"; exit 1; fi
	@echo "=== UBSan checks passed ==="

ubsan-pre: ubsan-pre-lib $(UBSAN_PRE_TARGETS)

# No special features needed: UBSan instruments the user binary at compile time;
# the preload only needs to intercept pthread calls, which it always does.
ubsan-pre-lib:
	cd .. && cargo build -p rsched-preload

$(UBSAN_PRE_TARGETS): %_pre: %.c $(PRELOAD)
	$(UBSAN_PRE_CC) $(UBSAN_PRE_CFLAGS) -o $@ $< $(UBSAN_PRE_LDFLAGS)

check-ubsan-pre: ubsan-pre
	@echo "=== ubsan_no_ub_pre: expect clean exit ==="
	$(SAN_TIMEOUT) ./ubsan_no_ub_pre
	@echo "=== ubsan_ub_pre: expect signed-integer-overflow report ==="
	@set +e; \
	env UBSAN_OPTIONS="halt_on_error=1" $(SAN_TIMEOUT) ./ubsan_ub_pre; \
	status=$$?; \
	if [ $$status -eq 124 ]; then echo "ubsan_ub_pre timed out"; exit 1; fi; \
	if [ $$status -eq 0 ]; then echo "ubsan_ub_pre unexpectedly exited cleanly"; exit 1; fi
	@echo "=== UBSan preload checks passed ==="

clean:
	rm -f $(TSAN_TARGETS) $(ASAN_TARGETS) $(ASAN_PRE_TARGETS) \
	      $(UBSAN_TARGETS) $(UBSAN_PRE_TARGETS) dist.txt
