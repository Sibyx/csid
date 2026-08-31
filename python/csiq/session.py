"""A typed view over the session block.

WHAT THE BLOCK IS. Opaque UTF-8 JSON. CSIQ does not constrain its schema — it is
whatever the capture tool recorded, and a reader that does not know a group must
ignore it. ``csid`` embeds its own sidecar (``csid-session/1``), which is what
these dataclasses describe.

WHY A TYPED VIEW IS SAFE HERE. Because it never replaces the block. Every view
keeps :attr:`Session.raw`, the mapping exactly as parsed, and every group keeps
its own ``raw`` too. Adding a group to the sidecar is not a format version bump,
so a typed view that *dropped* what it did not recognise would quietly turn a
forward-compatible container into a lossy one. Read a known field through the
attribute, and anything else through ``raw``.

THREE FIELDS THAT DO NOT MEAN WHAT THEY LOOK LIKE
-------------------------------------------------

* **``status: capturing`` is not a truncated capture.** Before csid 0.2.0 the
  export re-read the sidecar from disk, and a segmented capture deliberately
  leaves that file at ``capturing`` until the export lands. The embedded copy
  therefore said ``capturing`` forever, on effectively every segmented file in
  the archive. :attr:`Lifecycle.status_is_trustworthy` says whether the value
  can be believed.

* **An empty ``fingerprint`` is not "no filter".** ``no-filter`` means the radio
  filtered nothing. ``""`` means the field predates the group and nothing was
  recorded. Those are different facts and :attr:`Filter.filtering_known`
  separates them.

* **An all-empty ``build`` group is not a build with no revision.** It means the
  file predates build provenance entirely. Every such file reports
  ``csid_version = "0.1.0"``, because that literal was never bumped while the
  daemon gained injection, time transfer, segmentation, the BLE scanner and the
  empty-record counter. Those files cannot be distinguished by build, and no
  later pass can recover it.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Mapping

#: The csid version from which the embedded ``status`` states the real outcome.
STATUS_TRUSTWORTHY_FROM = (0, 2, 0)

#: What ``fingerprint`` carries when the radio filtered nothing at all.
NO_FILTER = "no-filter"


def _version_tuple(value: str) -> tuple[int, ...] | None:
    """``"0.2.1"`` -> ``(0, 2, 1)``; ``None`` when it is not a plain semver."""
    parts = value.split("-", 1)[0].split(".")
    try:
        return tuple(int(p) for p in parts)
    except ValueError:
        return None


@dataclass(frozen=True, slots=True)
class _Group:
    """Base for one sidecar group. ``raw`` is always the mapping as parsed."""

    raw: Mapping[str, Any] = field(default_factory=dict, repr=False)


@dataclass(frozen=True, slots=True)
class Identity(_Group):
    session_id: str = ""
    run_id: str = ""
    experiment: str = ""
    #: The capture PROFILE, not the experiment. A sidecar tag is never evidence
    #: that a session belongs to a given experiment card.
    tag: str = ""
    schema: str = ""


@dataclass(frozen=True, slots=True)
class Radio(_Group):
    interface: str = ""
    #: The monitor interface NAME, e.g. ``wlp1s0mon0``. Not a boolean — the
    #: spec's group table lists the field without its type, and a bool here
    #: silently coerces every real capture's interface name to True.
    monitor: str = ""
    band: str = ""
    channel: int = 0
    control_freq_mhz: int = 0
    center_freq_mhz: int = 0
    #: The configured MONITOR width. It bounds what the receiver could decode and
    #: describes no individual record — see ``CsiRecord.bandwidth_mhz`` for the
    #: frame's own bandwidth.
    width: str = ""
    interval_us: int = 0
    #: Normalised to a tuple. Real captures carry a JSON list; an empty tuple
    #: means the radio was told to accept every source address.
    mac_filter: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class Filter(_Group):
    frame_types: str = ""
    rate_n_flags_val: int = 0
    rate_n_flags_mask: int = 0
    count: int = 0
    timeout_us: int = 0
    #: Stable digest over (frame_types, rate_n_flags_val, rate_n_flags_mask).
    #: ``count`` and ``timeout_us`` are deliberately excluded: they bound how much
    #: the radio reports, not which frames it selects, so two captures differing
    #: only in duration stay poolable.
    fingerprint: str = ""

    @property
    def filtering_known(self) -> bool:
        """False when the selection was never recorded (empty fingerprint).

        ``no-filter`` is a recorded fact: the radio filtered nothing. ``""`` is
        the absence of a record. Pooling the two loses the distinction.
        """
        return self.fingerprint != ""

    @property
    def filtered(self) -> bool | None:
        """True/False when known, ``None`` when the selection was not recorded."""
        if not self.filtering_known:
            return None
        return self.fingerprint != NO_FILTER


@dataclass(frozen=True, slots=True)
class Build(_Group):
    revision: str = ""
    #: Read this FIRST. A build that cannot name its revision says ``none`` and
    #: leaves ``revision`` empty — it never guesses. That is the expected state
    #: when the source was deployed without its .git directory, which is how the
    #: capture fleet is built.
    revision_source: str = ""
    built_at: str = ""
    rustc: str = ""
    profile: str = ""
    csiq_format_version: int = 0

    @property
    def recorded(self) -> bool:
        """False when the file predates build provenance entirely."""
        return bool(self.revision_source or self.built_at or self.rustc)


@dataclass(frozen=True, slots=True)
class Environment(_Group):
    hostname: str = ""
    kernel: str = ""
    driver_module: str = ""
    firmware: str = ""
    regdomain: str = ""
    cpu_governor: str = ""
    csid_version: str = ""
    build: Build = field(default_factory=Build)


@dataclass(frozen=True, slots=True)
class Lifecycle(_Group):
    started_at: str = ""
    ended_at: str = ""
    status: str = ""
    #: Carried in from `environment`, because whether `status` can be believed is
    #: a property of the writer rather than of the lifecycle group.
    _csid_version: str = field(default="", repr=False)

    @property
    def status_is_trustworthy(self) -> bool:
        """Whether :attr:`status` states the capture's real outcome.

        A file written before csid 0.2.0 embeds whatever the on-disk sidecar said
        at export time, which for a segmented capture is ``capturing`` — forever.
        Such a value is not evidence the capture was truncated.
        """
        parsed = _version_tuple(self._csid_version)
        return parsed is not None and parsed >= STATUS_TRUSTWORTHY_FROM


@dataclass(frozen=True, slots=True)
class Summary(_Group):
    records: int = 0
    #: Absent on a pre-counter sidecar. ``None`` is not zero — a note that prints
    #: 0 here asserts no empty records were seen, which was never measured.
    empty_records: int | None = None
    capture_bytes: int = 0
    mean_rate_hz: float = 0.0
    tone_counts: Mapping[str, int] = field(default_factory=dict)
    live_dropped: int = 0

    @property
    def useful_records(self) -> int | None:
        """Records carrying a non-zero estimate, or ``None`` when not recorded."""
        if self.empty_records is None:
            return None
        return self.records - self.empty_records


@dataclass(frozen=True)
class Session:
    """The embedded session block, typed, with the original mapping retained."""

    raw: Mapping[str, Any] = field(default_factory=dict, repr=False)
    identity: Identity = field(default_factory=Identity)
    radio: Radio = field(default_factory=Radio)
    filter: Filter = field(default_factory=Filter)
    environment: Environment = field(default_factory=Environment)
    lifecycle: Lifecycle = field(default_factory=Lifecycle)
    summary: Summary = field(default_factory=Summary)

    def __repr__(self) -> str:
        sid = self.identity.session_id or "<unnamed>"
        return f"<Session {sid} {self.radio.band}GHz ch{self.radio.channel} {self.radio.width}>"

    @property
    def groups_not_typed(self) -> list[str]:
        """Top-level groups present in the block that this view does not type.

        Not an error. Adding a group is not a format version bump, so a newer
        writer legitimately produces groups this reader has never heard of. They
        are readable through :attr:`raw`; this property just names them.
        """
        known = {"identity", "radio", "filter", "environment", "lifecycle", "summary"}
        consumed = getattr(self, "_consumed", frozenset())
        return sorted(k for k in self.raw if k not in known and k not in consumed)

    @classmethod
    def from_mapping(cls, blob: Mapping[str, Any] | None) -> "Session":
        """Build the typed view. A missing block yields an all-default Session.

        **Groups are optional and the fields may be flat.** The spec's group
        table is how the sidecar is organised when it is organised; on every
        capture written before csid 0.2.0 the identity and lifecycle fields sit
        at the top level and there is no ``filter`` group at all. Each group
        therefore reads its own mapping when one exists and falls back to the
        root, so a flat block types correctly instead of returning empty.
        """
        if not blob:
            return cls()

        consumed: set[str] = set()

        def source(name: str) -> Mapping[str, Any]:
            """The group's own mapping, or the root when it is absent or null."""
            value = blob.get(name)
            if isinstance(value, Mapping):
                consumed.add(name)
                return value
            if name in blob:  # present but null — a real shape, e.g. "summary": null
                consumed.add(name)
            return blob

        def pick(cls_: Any, src: Mapping[str, Any], **extra: Any) -> Any:
            names = {f for f in cls_.__dataclass_fields__ if f != "raw" and not f.startswith("_")}
            kwargs: dict[str, Any] = {}
            for key, value in src.items():
                if key not in names:
                    continue
                # Mark consumed before the null check: a known field that is
                # explicitly null (``"ended_at": null`` on a running capture) is
                # recognised, not unknown. Leaving it out reported every null
                # field as a group this reader does not type.
                if src is blob:
                    consumed.add(key)
                if value is None:
                    continue
                kwargs[key] = value
            kwargs.update(extra)
            return cls_(raw=src, **kwargs)

        def macs(value: Any) -> tuple[str, ...]:
            if isinstance(value, str):
                return (value,) if value else ()
            if isinstance(value, (list, tuple)):
                return tuple(str(v) for v in value)
            return ()

        radio_raw = source("radio")
        env_raw = source("environment")
        build_raw = env_raw.get("build")
        build = pick(Build, build_raw if isinstance(build_raw, Mapping) else {})
        env_fields = {k: v for k, v in env_raw.items() if k != "build"}

        session = cls(
            raw=blob,
            identity=pick(Identity, source("identity")),
            radio=pick(Radio, radio_raw, mac_filter=macs(radio_raw.get("mac_filter"))),
            filter=pick(Filter, source("filter")),
            environment=pick(Environment, env_fields, build=build),
            lifecycle=pick(
                Lifecycle,
                source("lifecycle"),
                _csid_version=str(env_raw.get("csid_version") or ""),
            ),
            summary=pick(Summary, source("summary")),
        )
        object.__setattr__(session, "_consumed", frozenset(consumed))
        return session

__all__ = [
    "NO_FILTER",
    "STATUS_TRUSTWORTHY_FROM",
    "Build",
    "Environment",
    "Filter",
    "Identity",
    "Lifecycle",
    "Radio",
    "Session",
    "Summary",
]
