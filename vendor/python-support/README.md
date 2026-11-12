# Private Python package support

Build-time input for components that already use Python installation helpers.
Each product still packages and owns its interpreter. This module creates no
global Python, service, account or shared installation dependency.

Vendor this directory from an exact Constellation commit, recording that commit
and file hashes with the component. Do not fetch mutable `main` during a release.
The common lock is authoritative for these artifacts; change it here and qualify
consuming components before adopting a new revision.

```python
import python_support

metadata = python_support.prepare(target_triple, cache_directory, new_directory)
```

The function obtains the exact checksummed archive, resolves only internal file
aliases, omits mutable bytecode caches, and preserves distributor license notices.
It refuses a used destination. Package the resulting ordinary files with their
executable permissions; do not copy a developer virtual environment. Runtime
invocations should use the absolute private executable with `-I`, and `-B` when
the package is immutable. Python account, global PATH and package-manager setup
are unnecessary.

The lock comes from the distributor's actual release assets, including full
archive provenance for notices omitted from `install_only`. Preserve CPython and
dependency notices in redistributed packages. Windows includes private VC runtime
DLLs. The distributor declares glibc 2.17 for these Linux Python builds, macOS
10.15 for Intel and 11.0 for Apple Silicon. These are interpreter constraints, not
claims about the containing application's minimum OS or complete setup coverage.

Run `python packaging/python/test_support.py`. Consuming applications must also
test their packaged executable and private interpreter on each target, with
developer tools absent from PATH, and verify installation/restart continuity.

Primary source: [Python Build Standalone distributions](https://docs.astral.sh/python-build-standalone/distributions/).
