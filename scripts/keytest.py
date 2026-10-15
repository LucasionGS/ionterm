# Raw input logger for scripts/keytest.sh: switches kitty keyboard flags
# every 4 seconds and writes each read from the terminal to the log file.
import os, sys, termios, tty, time, select
fd = 0
old = termios.tcgetattr(fd)
tty.setraw(fd)
log = open(sys.argv[1], "w", buffering=1)
start = time.time()
phases = [0, 1, 11, 31]  # legacy, disambiguate, +events+all-escapes, everything
try:
    for flags in phases:
        os.write(1, f"\x1b[={flags};1u".encode())
        log.write(f"PHASE {flags} {time.time()-start:.2f}\n")
        end = time.time() + 4
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.1)
            if r:
                log.write(f"{time.time()-start:.2f} {os.read(fd, 256)!r}\n")
finally:
    os.write(1, b"\x1b[=0;1u")
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
