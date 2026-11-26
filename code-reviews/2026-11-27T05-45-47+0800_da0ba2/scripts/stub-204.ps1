param([int]$Port)
# 极简假控制面：对任何请求回 204 No Content。只用于探针。
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, $Port)
$listener.Start()
while ($true) {
    $conn = $listener.AcceptTcpClient()
    try {
        $stream = $conn.GetStream()
        $one = New-Object byte[] 1
        $head = ''
        while (-not $head.EndsWith("`r`n`r`n")) {
            $n = $stream.Read($one, 0, 1)
            if ($n -le 0) { break }
            $head += [char]$one[0]
        }
        $length = 0
        if ($head -match '(?im)^content-length:\s*(\d+)') { $length = [int]$Matches[1] }
        if ($length -gt 0) {
            $buf = New-Object byte[] $length
            $read = 0
            while ($read -lt $length) { $read += $stream.Read($buf, $read, $length - $read) }
        }
        $reply = [System.Text.Encoding]::ASCII.GetBytes("HTTP/1.1 204 No Content`r`nContent-Length: 0`r`nConnection: close`r`n`r`n")
        $stream.Write($reply, 0, $reply.Length)
        $stream.Flush()
    } finally { $conn.Close() }
}