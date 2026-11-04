# Points the current shell at the local magnet stack of ../magnet_server, with the debug build
# of this checkout: source dev-env.sh

if [ -n "$ZSH_VERSION" ]; then _magnet_root="${${(%):-%x}:A:h}"; else _magnet_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; fi

export PATH="$_magnet_root/target/debug:$PATH"
export MAGNET_GIT_INDEX_URL=git://localhost:9418/magnet_index.git
export MAGNET_HOME="$_magnet_root/target/beta-home" # keeps the real ~/.magnet out of it
export MAGNET_SCLANG=/Applications/SuperCollider-3.14.1.app/Contents/MacOS/sclang

unset _magnet_root
