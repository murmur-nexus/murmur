import inspect
import logging
import os

from ruamel.yaml import YAML

from .logging_config import configure_logging

configure_logging()
logger = logging.getLogger(__name__)


class InstructionsHandler:
    """A class to handle the retrieval of instructions for a given module.

    This class attempts to retrieve instructions using various methods in a specific order of priority:
    1. Provided instructions directly passed to the method.
    2. Instructions from the root manifest file 'murmur.yaml'.
    3. Instructions from the module-specific manifest file 'murmur-build.yaml'.
    4. Instructions from the module's attributes.

    Attributes:
        MANIFEST_FILE (str): The name of the root manifest file.
        BUILD_MANIFEST_FILE (str): The name of the module-specific manifest file.
    """

    MANIFEST_FILE: str = 'murmur.yaml'
    BUILD_MANIFEST_FILE: str = 'murmur-build.yaml'

    def get_instructions(self, module, provided_instructions: list[str] | None) -> str:
        """Try different methods to get instructions in order of priority."""
        instructions = (
            self._try_provided_instructions(provided_instructions)
            or self._try_root_manifest()
            or self._try_module_manifest(module)
            or self._try_module_attributes(module)
            or ''
        )
        return instructions

    def _try_provided_instructions(self, instructions: list[str] | None) -> str | None:
        """Check if provided instructions are available and return them."""
        logger.debug('Step 1: checking provided instructions')
        if instructions:
            return ' '.join(instructions)
        return None

    def _try_root_manifest(self) -> str | None:
        """Attempt to retrieve instructions from the root manifest file."""
        logger.debug("Step 2: checking root's murmur.yaml")
        try:
            project_root = self._find_project_root()
            config_path = os.path.join(project_root, self.MANIFEST_FILE)
            yaml = YAML(typ='safe')
            with open(config_path) as file:
                config = yaml.load(file)
                if config and 'instructions' in config:
                    return ' '.join(config['instructions'])
        except FileNotFoundError:
            pass
        return None

    def _try_module_manifest(self, module) -> str | None:
        """Attempt to retrieve instructions from the module-specific manifest file."""
        logger.debug("Step 3: checking module's murmur-build.yaml")
        try:
            module_path = os.path.dirname(inspect.getfile(module))
            module_config_path = os.path.join(module_path, self.BUILD_MANIFEST_FILE)
            logger.debug(f'module_config_path: {module_config_path}')
            yaml = YAML(typ='safe')
            with open(module_config_path) as file:
                config = yaml.load(file)
                if config and 'instructions' in config:
                    return ' '.join(config['instructions'])
        except (FileNotFoundError, TypeError, PermissionError) as e:
            logger.debug(f'Error loading module manifest: {str(e)}')
        return None

    def _try_module_attributes(self, module) -> str | None:
        """Attempt to retrieve instructions from the module's attributes."""
        logger.debug("Step 4: checking module's instructions list")
        try:
            if hasattr(module, 'instructions'):
                return ' '.join(module.instructions)
        except (AttributeError, TypeError):
            pass
        return None

    def _find_project_root(self) -> str:
        """Locates the project root directory containing murmur.yaml by traversing upwards from the caller's script directory."""
        # Inspect the call stack to find the caller's frame
        frame = inspect.currentframe()
        try:
            outer_frames = inspect.getouterframes(frame)
            # The caller's frame is typically two levels up:
            # [0] is _find_project_root
            # [1] is __init__
            # [2] is the caller (swarm_example.py)
            if len(outer_frames) < 3:
                raise FileNotFoundError(f"Cannot determine the caller's directory to locate {self.MANIFEST_FILE}.")
            caller_frame = outer_frames[2].frame
            caller_dir = os.path.dirname(os.path.abspath(caller_frame.f_code.co_filename))
        finally:
            del frame  # Prevent reference cycles

        current_dir = caller_dir
        while True:
            config_path = os.path.join(current_dir, self.MANIFEST_FILE)
            if os.path.isfile(config_path):
                return current_dir
            parent_dir = os.path.dirname(current_dir)
            if parent_dir == current_dir:
                # Reached the filesystem root without finding murmur.yaml
                raise FileNotFoundError(
                    f"Project root not found: '{self.MANIFEST_FILE}' must exist in the project root directory."
                )
            current_dir = parent_dir
