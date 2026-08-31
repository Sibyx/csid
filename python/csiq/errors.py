"""Typed errors for the CSIQ reader.

Every error a reader can raise is a subclass of :class:`CsiqError`, so a caller
that does not care why a file failed keeps one ``except`` clause. A caller that
does care gets a type rather than a substring match on a message.

Three of these encode a distinction the spec makes explicitly:

* :class:`UnsupportedVersion` — the spec says a reader meeting a ``version`` it
  does not implement **must refuse the file rather than guess**. Refusing is not
  the same as meeting a corrupt one.
* :class:`ZstdUnavailable` — the spec says a reader that meets a ``.zst`` and has
  no decoder **should say so in those terms** rather than report a corrupt
  container. That is a missing dependency, not a bad file.
* :class:`DesyncError` — the ``0xA1`` record tag is a framing check. A reader that
  meets any other byte where a tag belongs knows the stream has desynchronised
  and must stop rather than emit garbage.
"""

from __future__ import annotations


class CsiqError(Exception):
    """Base class for every CSIQ parsing failure."""


class BadMagic(CsiqError):
    """The first four bytes are not ``CSIQ``. This is not a CSIQ container."""


class UnsupportedVersion(CsiqError):
    """The container declares a format version this reader does not implement.

    The spec requires refusal rather than a guess, because a version bump means
    the meaning or the framing changed.
    """

    def __init__(self, found: int, supported: int) -> None:
        super().__init__(f"unsupported CSIQ version {found} (this reader handles v{supported})")
        self.found = found
        self.supported = supported


class TruncatedCapture(CsiqError):
    """The file ended in the middle of a structure that was still being read."""


class DesyncError(CsiqError):
    """A record tag was expected and something else was found.

    The stream has desynchronised. Records already yielded are good; nothing
    after this point can be trusted.
    """


class MissingRequiredField(CsiqError):
    """A record omitted a field the spec marks required.

    The required set is ``FTM``, ``NRX``, ``NTX`` and ``NTONE``. A reader must
    reject such a record rather than fill a default.
    """

    def __init__(self, missing: list[str]) -> None:
        super().__init__(f"record missing required field(s): {sorted(missing)}")
        self.missing = sorted(missing)


class ZstdUnavailable(CsiqError):
    """A ``.csiq.zst`` was opened without a zstd decoder available.

    The file is fine. Install the ``zstd`` extra, or Python 3.14's
    ``compression.zstd``, or the ``zstandard`` package.
    """


class NumpyUnavailable(CsiqError):
    """``matrix()`` was called without NumPy installed.

    Parsing works without it. Only the complex-array view needs it.
    """


class FieldNotRecorded(CsiqError):
    """An operation needs a field this capture's writer never emitted.

    Distinct from the field being absent on one record, which is often a fact in
    its own right — an absent ``MONO_US`` marks the node's own transmission, an
    absent ``NODE_*`` sample means the sparse series did not tick here. This
    error is the *other* case: no record carries the field, because the writer
    predates it, so any rule built on its presence is inapplicable rather than
    false.

    Raising beats returning an empty result. An empty iterator from
    ``received()`` reads as "this capture received nothing", which is a claim
    about the radio rather than about the writer.
    """

    def __init__(self, field: str, detail: str = "") -> None:
        message = f"no record in this capture carries {field}"
        if detail:
            message = f"{message}. {detail}"
        super().__init__(message)
        self.field = field


class MalformedField(CsiqError):
    """A TLV field's value does not match the length its type code requires."""


__all__ = [
    "BadMagic",
    "CsiqError",
    "DesyncError",
    "FieldNotRecorded",
    "MalformedField",
    "MissingRequiredField",
    "NumpyUnavailable",
    "TruncatedCapture",
    "UnsupportedVersion",
    "ZstdUnavailable",
]
