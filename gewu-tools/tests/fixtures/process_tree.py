import os
import signal

child = os.fork()
if child == 0:
    os.setpgid(0, 0)
    os.fork()
    print(os.getpgrp(), flush=True)
    while True:
        signal.pause()

os.waitpid(child, 0)
