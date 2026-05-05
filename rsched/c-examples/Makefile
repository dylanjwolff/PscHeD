LIB_DIR  := ../target/debug
LIB      := $(LIB_DIR)/librsched.a
INC      := ../include

CC       := gcc
CFLAGS   := -g -Wall -Wextra -I$(INC)
# Link our static library first so its symbols take precedence, then
# the system pthread / dl libs for anything we don't override.
LDFLAGS  := -L$(LIB_DIR) -Wl,--start-group $(LIB) -Wl,--end-group \
            -lpthread -ldl -lm

TARGETS  := uniform uniform-lock uaf

.PHONY: all clean lib

all: lib $(TARGETS)

lib:
	cd .. && cargo build

uniform: uniform.c $(LIB)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

uniform-lock: uniform-lock.c $(LIB)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

uaf: uaf.c $(LIB)
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

clean:
	rm -f $(TARGETS) dist.txt
