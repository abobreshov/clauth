# Vendored from Hermes Agent 0.19.0, `utils.py:380-404` (the `fast_safe_load`
# the tollgate projector imports), for the `hermes_projector_real` test tier
# only (hermes spec §7). Nothing else of Hermes is vendored: the tier runs the
# REAL projector against real PyYAML and python-dotenv, and this is the one
# Hermes function the projector calls.
#
# Hermes Agent is MIT licensed (`hermes_agent-0.19.0.dist-info/METADATA`,
# `License-Expression: MIT`); the notice below is its licence's condition for
# a copy of a substantial portion. The code is unchanged but for this header.
#
#   Permission is hereby granted, free of charge, to any person obtaining a
#   copy of this software and associated documentation files (the
#   "Software"), to deal in the Software without restriction, including
#   without limitation the rights to use, copy, modify, merge, publish,
#   distribute, sublicense, and/or sell copies of the Software, and to permit
#   persons to whom the Software is furnished to do so, subject to the
#   following conditions: The above copyright notice and this permission
#   notice shall be included in all copies or substantial portions of the
#   Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND.
from typing import Any

import yaml

#
# PyYAML's pure-Python SafeLoader is ~8x slower than the libyaml-backed
# ``CSafeLoader`` C extension. Startup parses config.yaml and every plugin
# manifest with the slow path, costing ~0.9s of cold-start time. The C loader
# is a true drop-in for ``safe_load`` (same restricted tag set), so prefer it
# and fall back to the pure-Python loader only when libyaml isn't compiled in.
_fast_yaml_loader = None


def _get_fast_yaml_loader():
    global _fast_yaml_loader
    if _fast_yaml_loader is None:
        _fast_yaml_loader = getattr(yaml, "CSafeLoader", None) or yaml.SafeLoader
    return _fast_yaml_loader


def fast_safe_load(stream: Any) -> Any:
    """``yaml.safe_load`` using the libyaml C loader when available.

    Accepts the same inputs as ``yaml.safe_load`` (a ``str``/``bytes`` document
    or a readable file object) and returns the same parsed structure. Falls
    back to PyYAML's pure-Python ``SafeLoader`` when ``CSafeLoader`` isn't
    available, so behavior is identical everywhere — only the speed differs.
    """
    return yaml.load(stream, Loader=_get_fast_yaml_loader())
