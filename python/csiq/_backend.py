"""Backend selection: the pure-Python parser, or the PyO3 accelerator.

The accelerator is optional and always has been. A platform with no wheel and no
Rust toolchain reads every file through the pure parser, more slowly and with
identical results — that equality is asserted by ``tests/test_backend_parity.py``
rather than assumed.

**The fast path is opt-out, not opt-in, when it is installed.** Set
``CSIQ_BACKEND=python`` to force the pure parser, which is what the parity test
does to get both sides in one process.
"""

from __future__ import annotations

import os
from typing import Any, Iterator, Optional

try:  # pragma: no cover - presence is environment-dependent
    import csiq_fast as _fast
except ImportError:  # pragma: no cover
    _fast = None  # type: ignore[assignment]


def available() -> bool:
    """Whether the compiled accelerator is importable."""
    return _fast is not None


def selected() -> str:
    """Which backend a fresh read would use: ``"rust"`` or ``"python"``."""
    if os.environ.get("CSIQ_BACKEND", "").strip().lower() == "python":
        return "python"
    return "rust" if available() else "python"


def fast_module() -> Any:
    """The accelerator module, or ``None``."""
    return _fast
