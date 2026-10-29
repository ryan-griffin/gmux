#!/usr/bin/env python3
"""Exercise real Bash/Zsh startup and semantic shell state over daemon streams."""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess

from clipboard_test_support import check_html
from cargo_test_support import cargo_binary
from progress_test_support import ProgressProbe
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

root = Path(__file__).resolve().parent.parent
binary = cargo_binary("gmux")


def directory_reporting_security():
    # Each shell has a new session, no controlling TTY, private startup files,
    # and captured output. Even a regression cannot reach a desktop clipboard.
    for shell in ("bash", "zsh"):
        executable = shutil.which(shell)
        assert executable, f"Install {shell} to run the shell integration suite"
        for locale in ("C", "C.UTF-8"):
            with AppFixture(prefix=f"gmux-{shell}-cwd-") as f:
                home = f.path / "home"
                home.mkdir()
                for name in (r"back\a\e]52;c;SGVsbG8=\a",
                             "raw\x07\x1b]52;c;SGVsbG8=\x07\r\n\t\x7f", "ordinary space 界"):
                    directory = home / name
                    directory.mkdir()
                    host = "host\x07\x1b]52;c;SGVsbG8=\x07\r\n\t\x7f" + r"\a\e"
                    script = root / "data/shell-integration" / (
                        "bash/ghostty.bash" if shell == "bash" else "zsh/ghostty-integration")
                    if shell == "bash":
                        command = (f"source {shlex.quote(str(script))} >/dev/null; "
                                   "_ghostty_executing=; BLE_VERSION=fixture; "
                                   f"HOSTNAME={shlex.quote(host)}; __ghostty_precmd")
                        flags = ["--noprofile", "--norc", "-ic"]
                        prefix = b"\x1b]133;P;k=i\x07"
                    else:
                        command = (f"source {shlex.quote(str(script))} >/dev/null; "
                                   "_ghostty_deferred_init >/dev/null 2>&1; _ghostty_fd=1; "
                                   f"HOST={shlex.quote(host)}; _ghostty_report_pwd")
                        flags = ["-fic"]
                        prefix = b""
                    env = dict(f.env, HOME=str(home), ZDOTDIR=str(home), ENV="",
                               LC_ALL=locale, TERM="dumb", GHOSTTY_SHELL_FEATURES="")
                    result = subprocess.run([executable, *flags, command], cwd=directory,
                                            env=env, capture_output=True, start_new_session=True,
                                            timeout=10, check=True)
                    clean = lambda text: "".join(c for c in text if ord(c) >= 32 and ord(c) != 127)
                    expected = prefix + b"\x1b]7;kitty-shell-cwd://" + (
                        clean(host) + clean(str(directory))).encode() + b"\x07"
                    assert result.stdout == expected, (shell, locale, repr(result.stdout))


with scenario("Bash/Zsh directory reports preserve backslashes and remove controls"):
    directory_reporting_security()


def shell_fixture(shell, array=False, no_history=False, relative_resources=False):
    executable = shutil.which(shell)
    assert executable, f"Install {shell} to run the shell integration suite"
    with AppFixture(prefix=f"gmux-{shell}-") as f:
        home = f.path / "home"
        home.mkdir()
        startup = f.path / "startup"
        hooks = f.path / "hooks"
        q = shlex.quote
        original_env = "" if array else "original-env"
        f.env.update(SHELL=executable, HOME=str(home), ENV=original_env,
                     ZDOTDIR=str(home), GMUX_RESOURCES_DIR="data" if relative_resources else str(root / "data"))
        f.env.pop("HISTFILE", None)
        if shell == "bash":
            (home / ".bash_profile").write_text(f"printf 'profile\\n' >> {q(str(startup))}\nsource ~/.bashrc\n")
            prompt_command = f"printf '%s\\n' \"$?\" >> {q(str(hooks))}"
            (home / ".bashrc").write_text(
                f"printf 'rc\\n' >> {q(str(startup))}\n"
                "PS1='GMUX-PROMPT> '\n"
                + ("HISTSIZE=0\n" if no_history else "")
                + (f"PROMPT_COMMAND=({q(prompt_command)})\n" if array else f"PROMPT_COMMAND={q(prompt_command)}\n")
                + f"PS0={q('$(echo preexec >> ' + q(str(hooks)) + ')')}\n"
                + "trap ':' DEBUG\n"
                + ('source "$GMUX_RESOURCES_DIR/shell-integration/bash/ghostty.bash"\n' if array else ""))
            script = '"$GMUX_RESOURCES_DIR/shell-integration/bash/ghostty.bash"'
        else:
            (home / ".zshenv").write_text(f"print env >> {q(str(startup))}\n")
            (home / ".zprofile").write_text(f"print profile >> {q(str(startup))}\n")
            (home / ".zshrc").write_text(
                f"print rc >> {q(str(startup))}\n"
                "PS1='GMUX-PROMPT> '\nRPROMPT='right'\nPS2='continued> '\n"
                f"precmd() {{ print $? >> {q(str(hooks))}; }}\n"
                f"preexec() {{ print preexec >> {q(str(hooks))}; }}\n")
            (home / ".zlogin").write_text(f"print login >> {q(str(startup))}\n")
            script = '"$GMUX_RESOURCES_DIR/shell-integration/zsh/ghostty-integration"'
        log = (f.path / "daemon.log").open("w")
        f.start(binary, "serve", cwd=root, stdout=log, stderr=log)
        wait_for(lambda: f.rpc("status"))
        pane = f.rpc("new", directory=str(home))["pane"]
        client = Client(f, pane)
        try:
            client.output("GMUX-PROMPT>")
            def meta():
                return f.rpc("metadata", pane=pane)["meta"]
            def expect_exit(code):
                return wait_for(lambda: (m := meta())["command"]["phase"] == "input"
                                and m["command"]["last_exit_code"] == code and m)
            assert meta()["command"]["phase"] == "input", meta()
            with scenario(f"{shell} login startup, hooks and exit status, array={array}, no_history={no_history}"):
                f.rpc("send", pane=pane, text="printf 'REAL_SHELL_OK\\n'; false", enter=True)
                m = expect_exit(1)
                assert not m["busy"], m
                assert m["command"]["command"] is None, m
                client.output("REAL_SHELL_OK")
                assert startup.read_text().splitlines() == (["profile", "rc"] if shell == "bash"
                                                            else ["env", "profile", "rc", "login"])
                assert "1" in hooks.read_text().splitlines(), hooks.read_text()
                assert "preexec" in hooks.read_text(), hooks.read_text()
                assert f.rpc("command-output", pane=pane)["text"] == "REAL_SHELL_OK"
                # Wait for the prompt hook so the unchanged status cannot make
                # this assertion pass before the empty line was processed.
                before = len(hooks.read_text().splitlines())
                f.rpc("send", pane=pane, text="", enter=True)
                wait_for(lambda: len(hooks.read_text().splitlines()) > before)
                expect_exit(1)
                assert f.rpc("command-output", pane=pane)["text"] == "REAL_SHELL_OK"
                own_output = f.path / "own-output"
                f.rpc("send", pane=pane, text=f"gmux command-output > {q(str(own_output))}", enter=True)
                wait_for(lambda: own_output.exists() and own_output.read_text() == "REAL_SHELL_OK\n")
                expect_exit(0)
            with scenario(f"{shell} repeated sourcing preserves prompt hooks and environment"):
                check = (f'[[ $ENV == {q(original_env)} && $HISTFILE == "$HOME/.bash_history" ]] && '
                         '''[[ $(trap -p DEBUG) == *"':' DEBUG"* ]]'''
                         if shell == "bash" else '[[ $ZDOTDIR == "$HOME" ]]')
                f.rpc("send", pane=pane, text=f"source {script}; source {script}; {check}", enter=True)
                expect_exit(0)
                f.rpc("send", pane=pane, text="printf 'AFTER_SOURCE\\n'; (exit 7)", enter=True)
                expect_exit(7)
                assert f.rpc("command-output", pane=pane)["text"] == "AFTER_SOURCE"
                assert "7" in hooks.read_text().splitlines()
            with scenario(f"{shell} multiline input, builtin busy state and command completion"):
                f.rpc("send", pane=pane, text="for word in a b; do\n printf '%s\\n' \"$word\"\ndone", enter=True)
                expect_exit(0)
                assert f.rpc("command-output", pane=pane)["text"] == "a\nb"
                # Use a distinct status so the preceding successful command
                # cannot satisfy this wait before the empty command finishes.
                f.rpc("send", pane=pane, text=":; (exit 23)", enter=True)
                expect_exit(23)
                f.rpc("send", pane=pane, text="printf 'BUILTIN_VISIBLE\\n'; read -r answer", enter=True)
                wait_for(lambda: (m := meta())["command"]["phase"] == "running" and m["busy"])
                assert not meta()["command"]["at_prompt"]
                # Zsh can echo this marker inside the command before printf
                # writes it. Wait for the actual output row, not that echo.
                client.until(lambda h: h["meta"]["id"] == pane and h["frame"]
                             and any(line.rstrip() == "BUILTIN_VISIBLE" for line in client.text().splitlines()))
                output = f.rpc("command-output", pane=pane)["text"]
                assert output == "BUILTIN_VISIBLE", (output, meta(), client.text())
                f.rpc("send", pane=pane, text="answer", enter=True)
                expect_exit(0)
            with scenario(f"{shell} reconnect retains command status, restart starts a fresh shell"):
                client.close()
                client = Client(f, pane)
                state = client.until(lambda h: h["meta"]["id"] == pane)["meta"]
                assert state["command"]["phase"] == "input", state
                # CLI reads the same metadata and plain command output contract.
                result = subprocess.check_output([str(binary), "metadata", "--pane", str(pane)], env=f.env)
                assert json.loads(result)["meta"]["command"]["phase"] == "input"
                f.rpc("send", pane=pane, text="false", enter=True)
                expect_exit(1)
                client.close()
                f.stop()
                f.start(binary, "serve", cwd=root, stdout=log, stderr=log)
                wait_for(lambda: f.rpc("status"))
                wait_for(lambda: meta()["command"]["phase"] == "input")
                assert meta()["command"]["last_exit_code"] is None, meta()
        finally:
            client.close()
            f.stop()
            log.close()


for shell, options in [("bash", {}), ("bash", dict(array=True)),
                       ("bash", dict(no_history=True, relative_resources=True)), ("zsh", {})]:
    shell_fixture(shell, **options)

with AppFixture(prefix="gmux-semantic-") as f:
    f.start(binary, "serve", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: f.rpc("status"))
    pane = f.rpc("new", directory=str(f.path))["pane"]
    probe = ProgressProbe(f, pane)
    first, second = Client(f, pane), Client(f, pane)
    try:
        first.send("resize", cols=16, rows=3)
        first.barrier(1)
        probe.write("\x1bc" + "".join(
            f"\x1b]133;A\x07$ \x1b]133;B\x07cmd{i}\r\n\x1b]133;C;cmdline_url=cmd{i}\x07result{i}\r\n\x1b]133;D;{i}\x07"
            for i in range(8)) + "\x1b]133;A\x07$ \x1b]133;B\x07")
        for client in [first, second]:
            client.output("result7")
        with scenario("external OSC markers, independent views and command-output copy"):
            first.send("scroll-to", row=0, serial=1)
            first.until(lambda h: h["frame"] and h["scroll_serial"] == 1)
            first.send("prompt", previous=False, serial=2)
            h = first.until(lambda h: h["frame"] and h["scroll_serial"] == 2)
            assert h["frame"]["scrollback"]["offset"] == 2, h
            first.send("select-output", point=[0, 3], copy="text-html", serial=1)
            h = first.until(lambda h: h.get("copy") is not None)
            check_html(h["copy"]["content"], "result1")
            second.send("copy")
            h = second.until(lambda h: h.get("error") is not None)
            assert "Select terminal text" in h["error"], h
            second.send("select-output", point=None, copy="text-html", serial=1)
            h = second.until(lambda h: h.get("copy") is not None)
            check_html(h["copy"]["content"], "result7")
            rich = json.loads(subprocess.check_output([str(binary), "command-output", "--pane", str(pane), "--html"], env=f.env, text=True))
            check_html(rich, "result7")
            result = subprocess.check_output([str(binary), "command-output", "--pane", str(pane)], env=f.env, text=True)
            assert result.rstrip("\n") == "result7", repr(result)
        with scenario("captured output rejects reused cells without affecting another client"):
            first.send("resize", cols=40, rows=10)
            first.barrier(2)
            probe.write("\x1bc\x1b]133;A\x07$ \x1b]133;B\x07old\r\n"
                        "\x1b]133;C\x07old output\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07")
            first.send("anchor-output", point=[0, 1])
            first.barrier(3)
            probe.write("\x1b[10;1H.\x1b[2J\x1b[H\x1b]133;A\x07$ \x1b]133;B\x07new\r\n"
                        "\x1b]133;C\x07new output\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07")
            first.send("select-output", point=None, anchored=True, copy="text", serial=2)
            h = first.until(lambda h: h.get("error") == "No command output available")
            assert h["selection_serial"] == 2 and h.get("copy") is None, h
            second.send("select-output", point=None, copy="text", serial=2)
            h = second.until(lambda h: h.get("copy") is not None)
            assert h["copy"]["content"]["text"] == "new output", h
        with scenario("captured output rejects erasure inside a later command"):
            probe.write("\x1bc\x1b]133;A\x07$ \x1b]133;B\x07old\r\n"
                        "\x1b]133;C\x07old output\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07")
            first.send("anchor-output", point=[0, 1])
            first.barrier(4)
            probe.write("new\r\n\x1b]133;C\x07\x1b[10;1H.\x1b[2J\x1b[Hnew first\r\nnew second\r\n"
                        "\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07")
            first.send("select-output", point=None, anchored=True, copy="text", serial=3)
            h = first.until(lambda h: h.get("error") == "No command output available")
            assert h["selection_serial"] == 3 and h.get("copy") is None, h
            second.send("anchor-output", point=[0, 0])
            second.barrier(5)
            second.send("select-output", point=None, anchored=True, copy="text", serial=3)
            h = second.until(lambda h: h.get("copy") is not None)
            assert h["copy"]["content"]["text"] == "new first\nnew second", h
        with scenario("partial events, alternate screen, reset and process fallback"):
            probe.burst(["\x1b]133;C;cmdline_", "url=partial%20command", "\x07"], .01)
            wait_for(lambda: f.rpc("metadata", pane=pane)["meta"]["command"]["command"] == "partial command")
            probe.write("\x1b[?1049h\x1b]133;D;99\x07\x1b]133;A\x07")
            first.send("select-output", point=None, copy=None, serial=4)
            h = first.until(lambda h: h.get("error") == "No command output available")
            assert h["selection_serial"] == 4, h
            assert f.rpc("metadata", pane=pane)["meta"]["command"]["phase"] == "running"
            probe.write("\x1b[?1049l\x1bc")
            wait_for(lambda: f.rpc("metadata", pane=pane)["meta"]["command"]["phase"] == "unknown")
            assert f.rpc("metadata", pane=pane)["meta"]["busy"], "Foreground process fallback was lost"
    finally:
        first.close()
        second.close()

print("PASS: real shell startup/hooks, lifecycle, output, views, reconnect, restoration and fallback")
