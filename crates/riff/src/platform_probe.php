$extensions = array();
foreach (get_loaded_extensions() as $extension) {
    $version = phpversion($extension);
    $extensions[strtolower($extension)] = $version === false ? "0" : (string) $version;
}
ksort($extensions);

$libraries = array();
if (defined('INTL_ICU_VERSION')) {
    $libraries['icu'] = (string) INTL_ICU_VERSION;
}
if (defined('LIBXML_DOTTED_VERSION')) {
    $libraries['libxml'] = (string) LIBXML_DOTTED_VERSION;
}
if (defined('OPENSSL_VERSION_TEXT')) {
    if (preg_match('/(?:OpenSSL|LibreSSL)\s+([^\s]+)/', OPENSSL_VERSION_TEXT, $matches)) {
        $libraries['openssl'] = $matches[1];
    }
}
if (defined('PCRE_VERSION')) {
    $libraries['pcre'] = preg_replace('/\s.*$/', '', (string) PCRE_VERSION);
}
if (defined('ZLIB_VERSION')) {
    $libraries['zlib'] = (string) ZLIB_VERSION;
}
if (function_exists('curl_version')) {
    $curl = curl_version();
    if (isset($curl['version'])) {
        $libraries['curl'] = (string) $curl['version'];
    }
}
ksort($libraries);

$trackedPaths = array();
$loadedIni = php_ini_loaded_file();
if (is_string($loadedIni) && $loadedIni !== '') {
    $trackedPaths[] = $loadedIni;
    $trackedPaths[] = dirname($loadedIni);
}
$scannedIni = php_ini_scanned_files();
if (is_string($scannedIni) && $scannedIni !== '') {
    foreach (preg_split('/,\s*/', trim($scannedIni)) as $iniFile) {
        if ($iniFile !== '') {
            $trackedPaths[] = $iniFile;
            $trackedPaths[] = dirname($iniFile);
        }
    }
}
if (defined('PHP_CONFIG_FILE_SCAN_DIR') && PHP_CONFIG_FILE_SCAN_DIR !== '') {
    foreach (explode(PATH_SEPARATOR, PHP_CONFIG_FILE_SCAN_DIR) as $scanDir) {
        if ($scanDir !== '') {
            $trackedPaths[] = $scanDir;
        }
    }
}
$trackedPaths = array_values(array_unique($trackedPaths));
sort($trackedPaths);

echo json_encode(array(
    'php_version' => PHP_VERSION,
    'php_version_id' => PHP_VERSION_ID,
    'int_size' => PHP_INT_SIZE,
    'zts' => defined('PHP_ZTS') && (bool) PHP_ZTS,
    'debug' => defined('PHP_DEBUG') && (bool) PHP_DEBUG,
    'ipv6' => defined('AF_INET6'),
    'extensions' => $extensions,
    'libraries' => $libraries,
    '_riff_tracked_paths' => $trackedPaths,
), JSON_UNESCAPED_SLASHES);
