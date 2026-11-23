"""tools/release.py's names, refusals and rewrites, with no registry, no
docker and no key. Standard library only:

    python3 -m unittest discover -s tools -p 'test_*.py'
"""

from __future__ import annotations

import datetime
import gzip
import hashlib
import io
import json
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import release as r  # noqa: E402

D = {"list": "sha256:" + "a" * 64, "amd64": "sha256:" + "b" * 64, "arm64": "sha256:" + "c" * 64}
TEMPLATE = """[[apps]]
id = "gha-runner"
name = "Runner for GitHub Actions"
version = ""
released = ""
compose_path = "apps/gha-runner/compose.yaml"
compose_sha256 = ""
[[apps.images]]
service = "broker"
image = "docker.io/eshork/gha-runner"
list = ""
amd64 = ""
arm64 = ""
[[apps.images]]
service = "runner"
image = "docker.io/eshork/gha-runner"
list = ""
amd64 = ""
arm64 = ""
[apps.permissions]
ports = ["8711:8711/tcp"]
"""
COMPOSE = """services:
  broker:
    image: docker.io/eshork/gha-runner:gha-runner-2026.1114.2
    ports:
      - "8711:8711/tcp"
  runner:
    image: docker.io/eshork/gha-runner:gha-runner-2026.1114.2
"""
OFFICIAL = """[channel]
name = "official"
key = "app-channel.pub"
[[apps]]
id = "fileferry"
image = "docker.io/eshork/fileferry"
[[apps]]
id = "gha-runner"
[[apps.images]]
service = "broker"
image = "docker.io/eshork/gha-runner"
"""
TRIAL = """app = "gha-runner"
id = "gha-runner-trial"
name = "Runner for GitHub Actions (trial)"
image = "docker.io/example/gha-runner-trial"
port = 8799
"""
IDENT = r.identity(TRIAL)


class Names(unittest.TestCase):
    def test_a_tag_is_its_prefix_a_day_and_a_count(self):
        self.assertTrue(r.tag_ok("gha-runner-2026.1121.1", "gha-runner"))
        self.assertFalse(r.tag_ok("gha-runner-2026.1321.1", "gha-runner"))
        self.assertFalse(r.tag_ok("gha-runner-2026.1100.1", "gha-runner"))
        self.assertFalse(r.tag_ok("gha-runner-2026.1121.0", "gha-runner"))
        self.assertFalse(r.tag_ok("gha-runner-trial-2026.1121.1", "gha-runner"))

    def test_next_is_the_days_next_free_count(self):
        day = datetime.date(2026, 11, 21)
        tags = ["x-2026.1121.1", "x-2026.1121.3", "x-2026.1120.9", "x-y-2026.1121.7"]
        self.assertEqual(r.next_tag("x", tags, day), "x-2026.1121.4")
        self.assertEqual(r.next_tag("x", [], day), "x-2026.1121.1")
        self.assertEqual(r.next_tag("x-y", tags, day), "x-y-2026.1121.8")

    def test_repositories_compare_as_docker_hub_resolves_them(self):
        for ref in ("eshork/x", "index.docker.io/eshork/x", "DOCKER.IO/eshork/x",
                    "index.docker.io:443/eshork/x", "registry-1.docker.io:443/eshork/x"):
            self.assertEqual(r.repository_name(ref), "docker.io/eshork/x")
        self.assertEqual(r.repository_name("ubuntu"), "docker.io/library/ubuntu")
        self.assertEqual(r.repository_name("ghcr.io/a/b"), "ghcr.io/a/b")

    def test_only_the_entrys_repository_is_written(self):
        r.only_repository("docker.io/a/b:t", "docker.io/a/b")
        with self.assertRaises(r.Refused):
            r.only_repository("docker.io/a/bc:t", "docker.io/a/b")


class Identities(unittest.TestCase):
    def test_an_identity_is_every_key_and_nothing_else(self):
        with self.assertRaisesRegex(r.Refused, "wants"):
            r.identity(TRIAL + 'extra = "x"\n')
        with self.assertRaisesRegex(r.Refused, "quote"):
            r.identity(TRIAL.replace("(trial)", '(tri\\"al)'))
        with self.assertRaisesRegex(r.Refused, "repository name"):
            r.identity(TRIAL.replace('id = "gha-runner-trial"', 'id = "Gha"'))
        with self.assertRaisesRegex(r.Refused, "port"):
            r.identity(TRIAL.replace("8799", "true"))

    def test_an_identity_that_collides_with_the_app_is_refused(self):
        self.assertEqual(r.identity_refusals(IDENT, TEMPLATE, OFFICIAL), [])
        cases = {
            'id = "gha-runner"': "the app's own",
            'image = "docker.io/eshork/gha-runner"': "isn't docker.io/<namespace>/",
            "port = 8711": "the app's own",
        }
        for change, said in cases.items():
            key = change.split(" ", 1)[0]
            text = "\n".join(
                change if line.startswith(f"{key} ") else line for line in TRIAL.splitlines()
            )
            got = "; ".join(r.identity_refusals(r.identity(text), TEMPLATE, OFFICIAL))
            self.assertIn(said, got, change)

    def test_an_identity_whose_image_is_official_is_refused(self):
        ident = r.identity(
            TRIAL.replace('id = "gha-runner-trial"', 'id = "gha-runner-x"').replace(
                "docker.io/example/gha-runner-trial", "eshork/gha-runner-x"
            )
        )
        official = OFFICIAL + '[[apps]]\nid = "x"\nimage = "eshork/gha-runner-x"\n'
        got = "; ".join(r.identity_refusals(ident, TEMPLATE, official))
        self.assertIn("is an official repository", got)

    def test_the_compose_takes_the_identitys_image_and_port_and_keeps_the_containers(self):
        c = r.identity_compose(COMPOSE, IDENT, "docker.io/eshork/gha-runner", (8711, 8711))
        c = r.compose_text(c, "gha-runner-trial-2026.1121.1", IDENT.image)
        self.assertIn('"8799:8711/tcp"', c)
        want = "docker.io/example/gha-runner-trial:gha-runner-trial-2026.1121.1"
        self.assertEqual(c.count(want), 2)
        self.assertNotIn("eshork/gha-runner", c)

    def test_the_entry_takes_the_identitys_names_and_compose_path(self):
        t = r.identity_template(TEMPLATE, IDENT, "trial/gha-runner-trial/compose.yaml")
        app = tomllib.loads(t)["apps"][0]
        self.assertEqual(
            (app["id"], app["name"], app["compose_path"], app["permissions"]["ports"]),
            (IDENT.id, IDENT.name, "trial/gha-runner-trial/compose.yaml", ["8799:8711/tcp"]),
        )
        self.assertEqual({i["image"] for i in app["images"]}, {IDENT.image})


class Entries(unittest.TestCase):
    def test_the_entry_is_filled_and_hashes_its_compose(self):
        e = r.entry_text(TEMPLATE, "gha-runner-2026.1121.1", D, "2026-11-21", COMPOSE)
        app = tomllib.loads(e)["apps"][0]
        self.assertEqual(app["version"], "gha-runner-2026.1121.1")
        self.assertEqual(app["released"], "2026-11-21")
        self.assertEqual(len(app["compose_sha256"]), 64)
        self.assertTrue(all(i["list"] == D["list"] for i in app["images"]))

    def test_the_catalog_keeps_its_head_and_other_apps_and_replaces_this_one(self):
        e = r.entry_text(TEMPLATE, "gha-runner-2026.1121.1", D, "2026-11-21", COMPOSE)
        once = r.channel_text(OFFICIAL, e)
        twice = r.channel_text(once, e)
        self.assertEqual(once, twice)
        ids = [a["id"] for a in tomllib.loads(twice)["apps"]]
        self.assertEqual(ids, ["fileferry", "gha-runner"])
        self.assertEqual(tomllib.loads(twice)["channel"]["key"], "app-channel.pub")

    def test_an_entrys_comment_is_replaced_with_it_never_copied_again(self):
        head = '[channel]\nname = "t"\nkey = "trial.pub"\n\n'
        other = '# Another app.\n[[apps]]\nid = "other"\n\n'
        mine = '# The trial entry.\n\n[[apps]]\nid = "trial"\nversion = "1"\n'
        stale = head + other + "# The trial entry.\n\n" * 3 + mine
        new = '# The trial entry.\n\n[[apps]]\nid = "trial"\nversion = "2"\n'
        once = r.channel_text(stale, new)
        self.assertEqual(once.count("# The trial entry."), 1)
        self.assertEqual(once.count("# Another app."), 1)
        self.assertEqual(r.channel_text(once, new), once)
        self.assertEqual([a["id"] for a in tomllib.loads(once)["apps"]], ["other", "trial"])
        self.assertEqual(tomllib.loads(once)["apps"][1]["version"], "2")

    def test_the_catalog_names_its_own_key_file(self):
        self.assertEqual(r.catalog_key(OFFICIAL), "app-channel.pub")
        with self.assertRaises(r.Refused):
            r.catalog_key('[channel]\nkey = "../x.pub"\n')
        with self.assertRaises(r.Refused):
            r.catalog_key("[channel]\n")

    def test_a_list_needs_both_architectures(self):
        index = {
            "mediaType": r.INDEX_TYPES[0],
            "manifests": [
                {"digest": D["amd64"], "platform": {"architecture": "amd64"}},
                {"digest": D["arm64"], "platform": {"architecture": "arm64"}},
            ],
        }
        self.assertEqual(r.digests_of_index(index, D["list"]), D)
        index["manifests"].pop()
        with self.assertRaises(r.Refused):
            r.digests_of_index(index, D["list"])


class Keys(unittest.TestCase):
    def test_the_key_must_be_the_catalogs_own(self):
        self.assertEqual(r.key_refusals(False, "trial.toml", b"T\n", b"T", b"A"), [])
        got = r.key_refusals(False, "trial.toml", b"X", b"T", b"A")
        self.assertIn("isn't the key the catalog names", got[0])

    def test_the_app_channel_key_signs_official_toml_alone(self):
        self.assertEqual(r.key_refusals(True, "official.toml", b"A", b"A", b"A"), [])
        got = "; ".join(r.key_refusals(False, "trial.toml", b"A\n", b"A", b"A"))
        self.assertIn("signs official.toml alone", got)

    def test_a_reformatted_pem_is_the_same_key(self):
        pem = b"-----BEGIN PUBLIC KEY-----\nQUJDREVGR0g=\n-----END PUBLIC KEY-----\n"
        crlf = b"-----BEGIN PUBLIC KEY-----\r\nQUJD\r\nREVGR0g=\r\n-----END PUBLIC KEY-----\r\n"
        got = "; ".join(r.key_refusals(False, "trial.toml", pem, pem, crlf))
        self.assertIn("signs official.toml alone", got)

    def test_official_toml_takes_no_other_key(self):
        got = "; ".join(r.key_refusals(True, "official.toml", b"T", b"T", b"A"))
        self.assertIn("signed with the app-channel key alone", got)

    def test_no_app_channel_pub_to_compare_is_refused(self):
        got = "; ".join(r.key_refusals(False, "trial.toml", b"T", b"T", None))
        self.assertIn("no app-channel.pub", got)

    def test_an_official_image_goes_into_official_toml_alone(self):
        self.assertIsNone(r.official_image_refusal(True, "eshork/fileferry", OFFICIAL))
        self.assertIsNotNone(r.official_image_refusal(False, "eshork/fileferry", OFFICIAL))
        self.assertIsNone(r.official_image_refusal(False, IDENT.image, OFFICIAL))


class Catalogs(unittest.TestCase):
    """Which file is official.toml is its resolved path's answer, not its name's."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()
        (self.root / "official.toml").write_text(OFFICIAL)
        (self.root / "lab").mkdir()
        (self.root / "lab/official.toml").write_text(OFFICIAL)
        (self.root / "trial.toml").write_text("x")
        (self.root / "linked.toml").symlink_to(self.root / "official.toml")

    def tearDown(self):
        self.tmp.cleanup()

    def test_only_the_roots_official_toml_is_official(self):
        self.assertTrue(r.is_official(self.root / "official.toml", self.root))
        self.assertTrue(r.is_official(self.root / "linked.toml", self.root))
        (self.root / "hard.toml").hardlink_to(self.root / "official.toml")
        self.assertTrue(r.is_official(self.root / "hard.toml", self.root))
        if (self.root / "OFFICIAL.TOML").exists():  # a case-insensitive file system
            self.assertTrue(r.is_official(self.root / "OFFICIAL.TOML", self.root))
        self.assertFalse(r.is_official(self.root / "lab/official.toml", self.root))
        self.assertFalse(r.is_official(self.root / "trial.toml", self.root))

    def test_a_catalog_in_a_folder_is_refused(self):
        got = "; ".join(r.catalog_refusals(self.root / "lab/official.toml", self.root, False))
        self.assertIn("lives at the repository's root", got)
        self.assertEqual(r.catalog_refusals(self.root / "trial.toml", self.root, True), [])

    def test_a_linked_catalog_is_refused(self):
        got = "; ".join(r.catalog_refusals(self.root / "linked.toml", self.root, False))
        self.assertIn("a catalog is a file, not a link", got)

    def test_an_identity_never_goes_into_official_toml(self):
        for name in ("official.toml", "linked.toml"):
            got = "; ".join(r.catalog_refusals(self.root / name, self.root, True))
            self.assertIn("never goes into official.toml", got)


class Secrets(unittest.TestCase):
    ENV = {
        "PATH": "/usr/bin", "HOME": "/h", "SIGNING_KEY": "k", "COSIGN_PASSWORD": "p",
        "REGISTRY_AUTH_FILE": "/a/config.json", "DOCKER_CONFIG": "/a", "DOCKERHUB_TOKEN": "t",
        "GITHUB_TOKEN": "g", "COSIGN_ANYTHING": "x",
    }  # fmt: skip

    def test_the_scrubbed_environment_holds_no_secret(self):
        self.assertEqual(r.scrubbed_env(self.ENV), {"PATH": "/usr/bin", "HOME": "/h"})

    def test_the_secrets_held_are_named(self):
        self.assertEqual(r.secrets_held({"PATH": "x", "SIGNING_KEY": "", "COSIGN_PASSWORD": "p"}),
                         ["COSIGN_PASSWORD"])  # fmt: skip

    def test_next_skips_the_registrys_tags_too(self):
        day = datetime.date(2026, 11, 22)
        git = ["gha-runner-trial-2026.1122.1"]
        hub = ["gha-runner-trial-2026.1122.2", "sha256-ab.sig"]
        self.assertEqual(r.next_tag("gha-runner-trial", git + hub, day), "gha-runner-trial-2026.1122.3")


class Logins(unittest.TestCase):
    def test_skopeos_login_alone_is_refused_for_cosign(self):
        why = r.cosign_login_refusal({"auths": {"docker.io": {"auth": "x"}}})
        self.assertIn("https://index.docker.io/v1/", why)
        self.assertNotIn('"x"', why)

    def test_cosigns_key_a_store_or_a_helper_will_do(self):
        for config in (
            {"auths": {"docker.io": {}, "https://index.docker.io/v1/": {}}},
            {"credsStore": "desktop"},
            {"credHelpers": {"index.docker.io": "x"}},
        ):
            self.assertIsNone(r.cosign_login_refusal(config), config)


class Binaries(unittest.TestCase):
    def test_a_binary_that_names_a_local_folder_is_caught(self):
        clean = {"a": b"\x00/src/main.rs\x00"}
        dirty = {"b": b"\x00/Volumes/X/work/src/main.rs\x00"}
        root = {"c": b"\x00/opt/ci/work/main.rs\x00"}
        self.assertEqual(r.local_paths(clean, []), [])
        self.assertTrue(r.local_paths(dirty, []))
        self.assertTrue(r.local_paths(root, ["/opt/ci/work"]))

    def test_the_broker_is_read_from_each_architectures_last_layer(self):
        with tempfile.TemporaryDirectory() as d:
            archive, digests = oci_archive(Path(d), {
                "amd64": [{"usr/local/bin/b": b"old"}, {"usr/local/bin/b": b"amd"}],
                "arm64": [{"./usr/local/bin/b": b"arm"}, {"etc/x": b"x"}],
            })  # fmt: skip
            got = r.archive_file(archive, digests, "usr/local/bin/b")
            self.assertEqual(got, {"amd64/b": b"amd", "arm64/b": b"arm"})
            with self.assertRaises(r.Refused):
                r.archive_file(archive, digests, "usr/local/bin/none")

    def test_the_build_is_the_apps_folder_with_its_dockerfile(self):
        cmd = r.build_command("t-2026.1121.1", Path("/o.tar"), Path("/app"), "b")
        self.assertEqual(cmd[:5], ["docker", "buildx", "build", "--builder", "b"])
        for want in ("--provenance=false", "--sbom=false", "APP_RELEASE=t-2026.1121.1",
                     "linux/amd64,linux/arm64"):  # fmt: skip
            self.assertIn(want, cmd)
        self.assertEqual(cmd[-3:], ["-f", "/app/image/Dockerfile", "/app"])
        self.assertNotIn("--builder", r.build_command("t", Path("/o"), Path("/c"), None))

    def test_no_cargo_runs_on_this_machine(self):
        """#821: the broker is built in the Dockerfile's stage, in BuildKit."""
        src = Path(r.__file__).read_text()
        self.assertNotIn('"cargo"', src)
        dockerfile = Path(__file__).resolve().parents[1] / "apps/gha-runner/image/Dockerfile"
        text = dockerfile.read_text()
        self.assertIn("AS broker", text)
        self.assertIn("RUN --network=none", text)
        self.assertIn("COPY --from=broker", text)


def oci_archive(d: Path, layers: dict[str, list[dict[str, bytes]]]) -> tuple[Path, dict[str, str]]:
    """A small OCI archive: each architecture's manifest and gzipped layers."""
    blobs: dict[str, bytes] = {}

    def put(raw: bytes) -> str:
        digest = "sha256:" + hashlib.sha256(raw).hexdigest()
        blobs[digest] = raw
        return digest

    def tar_of(files: dict[str, bytes]) -> bytes:
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w") as t:
            for name, data in files.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                t.addfile(info, io.BytesIO(data))
        return gzip.compress(buf.getvalue())

    digests = {}
    for arch, ls in layers.items():
        manifest = {"layers": [{"digest": put(tar_of(f))} for f in ls]}
        digests[arch] = put(json.dumps(manifest).encode())
    archive = d / "a.oci.tar"
    with tarfile.open(archive, "w") as t:
        for digest, raw in blobs.items():
            info = tarfile.TarInfo(f"blobs/sha256/{digest.removeprefix('sha256:')}")
            info.size = len(raw)
            t.addfile(info, io.BytesIO(raw))
    return archive, digests


class Run(unittest.TestCase):
    """The command's refusals before any build, in a throwaway repository."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        files = {
            "official.toml": OFFICIAL,
            "app-channel.pub": "A\n",
            "trial.toml": '[channel]\nname = "t"\nkey = "trial.pub"\n',
            "trial.pub": "T\n",
            "trial/gha-runner.toml": TRIAL,
            "apps/gha-runner/entry.toml": TEMPLATE,
            "apps/gha-runner/compose.yaml": COMPOSE,
        }
        for name, text in files.items():
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            (self.root / name).write_text(text)
        git = ["git", "-C", str(self.root)]
        subprocess.run([*git, "init", "-q"], check=True)
        # Its own identity: a hosted runner has none, and git var asks for it.
        subprocess.run([*git, "config", "user.name", "t"], check=True)
        subprocess.run([*git, "config", "user.email", "t@t"], check=True)
        subprocess.run([*git, "add", "-A"], check=True)
        subprocess.run(
            [*git, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "x"], check=True
        )

    def tearDown(self):
        self.tmp.cleanup()

    def commit(self):
        git = ["git", "-C", str(self.root)]
        subprocess.run([*git, "add", "-A"], check=True)
        subprocess.run(
            [*git, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "y"], check=True
        )

    def run_it(self, *argv):
        import contextlib
        import io
        import os

        out = io.StringIO()
        here = os.getcwd()
        os.chdir(self.root)
        try:
            with contextlib.redirect_stdout(out):
                code = r.main(list(argv))
        finally:
            os.chdir(here)
        return code, out.getvalue()

    def test_an_official_image_into_another_catalog_is_refused(self):
        code, out = self.run_it("apps/gha-runner", "--catalog", "trial.toml", "--no-push")
        self.assertEqual(code, 2)
        self.assertIn("released into official.toml alone", out)

    def test_a_tag_of_another_id_is_refused(self):
        code, out = self.run_it(
            "apps/gha-runner", "--catalog", "trial.toml", "--as", "trial/gha-runner.toml",
            "--tag", "gha-runner-2026.1121.1", "--no-push",
        )  # fmt: skip
        self.assertEqual(code, 2)
        self.assertIn("the tag is gha-runner-trial-YYYY.MMDD.N", out)

    def test_a_dirty_tree_is_refused(self):
        (self.root / "stray").write_text("x")
        code, out = self.run_it("apps/gha-runner", "--catalog", "official.toml", "--no-push")
        self.assertEqual(code, 2)
        self.assertIn("uncommitted changes", out)

    def test_a_catalog_named_official_toml_in_a_folder_is_refused(self):
        (self.root / "trial/official.toml").write_text(OFFICIAL)
        self.commit()
        code, out = self.run_it("apps/gha-runner", "--catalog", "trial/official.toml", "--no-push")
        self.assertEqual(code, 2)
        self.assertIn("lives at the repository's root", out)

    def test_an_identity_into_official_toml_is_refused(self):
        code, out = self.run_it(
            "apps/gha-runner", "--catalog", "official.toml", "--as", "trial/gha-runner.toml",
            "--no-push",
        )  # fmt: skip
        self.assertEqual(code, 2)
        self.assertIn("never goes into official.toml", out)

    def test_the_build_step_refuses_to_hold_a_secret(self):
        import os

        os.environ["COSIGN_PASSWORD"] = "p"
        try:
            code, out = self.run_it(
                "apps/gha-runner", "--catalog", "trial.toml", "--as", "trial/gha-runner.toml",
                "--build-only", "--out", str(self.root / "out"),
            )  # fmt: skip
        finally:
            os.environ.pop("COSIGN_PASSWORD")
        self.assertEqual(code, 2)
        self.assertIn("the build step holds secrets (", out)
        self.assertIn("COSIGN_PASSWORD", out)
        self.assertNotIn("PLAN", out)

    def test_publishing_needs_the_build_steps_record(self):
        code, out = self.run_it(
            "apps/gha-runner", "--catalog", "trial.toml", "--as", "trial/gha-runner.toml",
            "--from", str(self.root / "nothing"),
        )  # fmt: skip
        self.assertEqual(code, 2)
        self.assertIn("run the build step", out)

    def test_git_runs_without_secrets_or_hooks(self):
        import os

        hook = self.root / ".git/hooks/pre-commit"
        hook.write_text("#!/bin/sh\necho \"$SIGNING_KEY\" > leaked\nexit 1\n")
        hook.chmod(0o755)
        os.environ["SIGNING_KEY"] = "k"
        try:
            (self.root / "f").write_text("x")
            r._git(self.root, "add", "f")
            r._git(self.root, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "f")
            env = r._git(self.root, "var", "GIT_COMMITTER_IDENT")
        finally:
            os.environ.pop("SIGNING_KEY")
        self.assertFalse((self.root / "leaked").exists())
        self.assertTrue(env)

    def test_a_push_without_a_key_is_refused_before_it_builds(self):
        import os

        os.environ.pop("SIGNING_KEY", None)
        code, out = self.run_it(
            "apps/gha-runner", "--catalog", "trial.toml", "--as", "trial/gha-runner.toml",
        )  # fmt: skip
        self.assertEqual(code, 2)
        self.assertTrue("SIGNING_KEY is empty" in out or "cosign isn't on PATH" in out, out)
        self.assertNotIn("PLAN", out)


if __name__ == "__main__":
    unittest.main()
