#!/usr/bin/env python3
"""Real Core private-entry checks; --enabled requires the Linux opt-in build."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument("core")
parser.add_argument("--enabled", action="store_true")
options=parser.parse_args()
if not __debug__:
    raise SystemExit("Run without Python -O")
core=str(Path(options.core).resolve())
with tempfile.TemporaryDirectory(prefix="core-js-startup-") as directory:
    root=Path(directory)
    env={key:directory for key in ("HOME","XDG_CONFIG_HOME","XDG_DATA_HOME","XDG_CACHE_HOME")}
    def early(args, error):
        child=subprocess.Popen([core,*args],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,cwd=root,env=env)
        try:
            # Deliberately keep stdin open: unavailable/invalid modes must not read it.
            child.wait(timeout=5)
            stdout,stderr=child.communicate(timeout=1)
            assert child.returncode==2 and json.loads(stdout)=={"ok":False,"error":error}, (child.returncode,stdout,stderr)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate(timeout=1)
    early(["--internal-extension-js","extra"],"invalid_helper_arguments")
    early(["--internal-extension-js","--data-dir",str(root/"must-not-create")],"invalid_helper_arguments")
    if options.enabled:
        result=subprocess.run([core,"--internal-extension-js"],input=b'{"source":"export async function run(){return null}","input":null}',capture_output=True,timeout=5,cwd=root,env=env)
        assert result.returncode==0 and json.loads(result.stdout)=={"ok":True,"result":None}, (result.returncode,result.stdout,result.stderr)
    else:
        early(["--internal-extension-js"],"runner_unavailable")
    # No reader exists before spawn, avoiding a race when checking BrokenPipe.
    read_fd,write_fd=os.pipe()
    os.close(read_fd)
    try:
        broken=subprocess.Popen([core,"--internal-extension-js","extra"],stdin=subprocess.DEVNULL,stdout=write_fd,stderr=subprocess.PIPE,cwd=root,env=env)
    finally:
        os.close(write_fd)
    try:
        broken.communicate(timeout=5)
        assert broken.returncode==1,broken.returncode
    finally:
        if broken.poll() is None:
            broken.kill()
            broken.communicate(timeout=1)
    help_result=subprocess.run([core,"--help"],capture_output=True,timeout=5,cwd=root,env=env)
    assert help_result.returncode==0 and b"--internal-extension-js" not in help_result.stdout
    assert list(root.iterdir())==[],list(root.iterdir())
print("PASS Core private entry: strict arguments, immediate refusal or JSON null, broken output, public help and no startup files")
