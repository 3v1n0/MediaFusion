#!/bin/sh

set -e

profilePath="${PROFILE_PATH:-/config}"
downloadsPath="${DOWNLOADS_PATH:-/downloads}"
qbtConfigFile="$profilePath/qBittorrent/config/qBittorrent.conf"
htpasswdPath="/etc/apache2/.htpasswd"
authConf="/etc/apache2/conf.d/010-webdav-auth.conf"

# Ensure the configuration and download directories exist, this has to happen
# before the chown below or the config subdirectory stays root owned
mkdir -p "$profilePath" "$(dirname "$qbtConfigFile")" "$downloadsPath"

# httpd refuses to start with "httpd (pid N) already running" when a pid file
# survived a kill, drop the leftovers before handing over to supervisord
rm -f /run/apache2/httpd.pid /run/supervisord.pid

# downloads are populated by the host user, only fix the top level directory
# like the upstream entrypoint does
chown qbtUser:qbtUser "$downloadsPath"
chown -R qbtUser:qbtUser "$profilePath" /var/log/apache2 /run/apache2

# Custom logic to handle qBittorrent configuration setup
if [ ! -f "$qbtConfigFile" ]; then
    echo "Creating qBittorrent configuration file at $qbtConfigFile"
    cat << EOF > "$qbtConfigFile"
[BitTorrent]
Session\DefaultSavePath=$downloadsPath
Session\Port=6881
Session\TempPath=$downloadsPath/temp

[LegalNotice]
Accepted=true
EOF
fi

# write the WebDAV access rules from scratch on every start, editing the vhost
# in place would stack up duplicate directives on each restart
rm -f "$authConf"
if [ -f "$htpasswdPath" ]; then
    echo "Basic auth password file found. Configuring WebDAV with basic auth."
    cat > "$authConf" <<EOF
<Directory $downloadsPath>
    AuthType Basic
    AuthName "WebDAV"
    AuthUserFile $htpasswdPath
    Require valid-user
</Directory>
EOF
else
    echo "No basic auth password file found. Configuring WebDAV without basic auth."
    cat > "$authConf" <<EOF
<Directory $downloadsPath>
    Require all granted
</Directory>
EOF
fi

exec /usr/bin/supervisord -c /etc/supervisord.conf
