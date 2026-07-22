# csiq — Python reference reader

Reader for the [CSIQ Interchange Format v1](../docs/CSIQ-format-v1.md), the
self-describing container written by [`csid`](https://github.com/Sibyx/csid).

The parser is pure standard library; NumPy is optional and only needed to
materialise CSI matrices.

```python
from csiq import read_csiq, read_raw, decode_live

session, records = read_csiq("capture.csiq")
for rec in records:
    H = rec.matrix()          # complex [ntone, nrx*ntx]
    print(rec.ftm, rec.ntone, rec.rssi, rec.phy)

# the lossless driver-native stream, if you want the source of truth
for rec in read_raw("capture.raw", width="80MHz"):
    ...
```

Install: `pip install -e python[numpy]`
