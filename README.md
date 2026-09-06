# osaio-proxy

minimal proxy that logs into an osaio account and restreams all cameras as separate video and audio streans.

> [!WARNING]
> this project does not publish the api details, you can get them from the android app.

## verified cameras

| vendor | model | limitations |
|---|---|---|
| GNCC | GC3 | none |

## configuration

edit `config.toml`:

```toml
[osaio]
appid = "xxx"
app_secret = "xxx"
user_agent = "xxx"
global_base_url = "https://xxx/v2"

[account]
email = "your-email@example.com"
password = "your-password"

[server]
bind = "0.0.0.0:8080"
```

custom config path:
```sh
cargo run -- path/to/config.toml
```

## endpoints

| endpoint | description |
|---|---|
| `GET /cameras/info` | list all cameras with stream and settings urls |
| `GET /cameras/<id>/stream/video` | mpeg-ts video stream |
| `GET /cameras/<id>/stream/audio` | raw aac audio stream |
| `GET /cameras/<id>/settings` | all settings and their current values |
| `GET /cameras/<id>/settings/<name>` | get a single setting |
| `GET /cameras/<id>/settings/<name>/<value>` | set a setting |

## settings

| name | values |
|---|---|
| `night-vision` | `off`, `on`, `auto` |
