import logging
import os


def configure_logging():
    """Configure logging based on environment variables."""
    logging.basicConfig(
        level=logging.DEBUG if os.getenv('MURMUR_DEBUG_MODE', 'false').lower() in ['1', 'true'] else logging.WARNING,
        format='%(levelname)s: %(message)s',
    )
