"""Fast JSON for Python, written in Rust against the CPython C API.

``loads``, ``dumps`` (bytes) and ``dumps_str`` (str); see ``help(rjson.dumps)``.
Command line: ``rjson`` / ``python -m rjson`` (``python -m rjson.tool``), a
faster ``python -m json.tool``.
"""

from .rjson import *  # noqa: F403 (the extension's __all__)
from .rjson import __all__, __version__  # noqa: F401
