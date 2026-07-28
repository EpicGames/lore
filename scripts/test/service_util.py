import platform


LORE_SERVICE_ENVIRONMENT = {"LORE_USE_SERVICE": "1"}


def service_supported():
    return platform.system() in ("Windows", "Linux", "Darwin")
