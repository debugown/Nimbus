//! Texts made in Rust (the sidebar's fixed rows, statuses). The window's own texts are translated by
//! Slint's `@tr` and the files in `lang/`; these go through the table below instead. The source
//! language is Russian, like everywhere else; with English chosen, a text in the table is swapped,
//! one that isn't stays Russian until it is added.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::LazyLock;

// Tests check the source (Russian) texts, so they start without the swap.
static ENGLISH: AtomicBool = AtomicBool::new(!cfg!(test));

const TABLE: &[(&str, &str)] = &[
    ("Друзья", "Friends"),
    ("Запросы общения", "Message requests"),
    ("Личные сообщения", "Direct Messages"),
    ("В сети", "Online"),
    ("Подключение…", "Connecting…"),
    ("Профиль сохранён", "Profile saved"),
    ("«О себе» сохранено", "\"About me\" saved"),
    ("Не удалось открыть файл", "Could not open the file"),
    ("Не удалось прочитать файл", "Could not read the file"),
    ("Картинка больше 8 МБ", "The picture is bigger than 8 MB"),
    ("Нужна картинка PNG, JPEG, GIF или WebP", "A PNG, JPEG, GIF or WebP picture is needed"),
    ("Нет связи: показаны сохранённые сообщения, они могли устареть", "No connection: showing saved messages, they may be out of date"),
    ("Голос подключён", "Voice connected"),
    ("В звонке в личных сообщениях показ экрана пока недоступен", "Screen sharing is not available in DM calls yet"),
    ("Стримы в звонках в личных сообщениях пока не открываются", "Streams in DM calls can't be opened yet"),
    ("Не удалось позвонить: ", "Could not ring: "),
    ("Голосовое сообщение лежит не на сервере Discord", "The voice message isn't on Discord's servers"),
    ("Не удалось скачать голосовое сообщение", "Could not download the voice message"),
    ("Не удалось воспроизвести голосовое сообщение", "Could not play the voice message"),
    ("В сети:", "Online:"),
    ("Участников:", "Members:"),
    ("Это не похоже на приглашение Discord", "This doesn't look like a Discord invite"),
    ("Приглашение недействительно или истекло", "The invite is invalid or has expired"),
    ("Discord просит пройти проверку. Войдите по приглашению в официальном клиенте", "Discord asks for verification. Join with this invite in the official client"),
    ("Название должно быть от 1 до 100 символов", "The name must be 1 to 100 characters"),
    ("Заголовок должен быть от 1 до 100 символов", "The title must be 1 to 100 characters"),
    ("Текст должен быть от 1 до 2000 символов", "The text must be 1 to 2000 characters"),
    ("Discord не вернул новую ветку", "Discord did not return the new thread"),
    ("Сначала откройте чат", "Open a chat first"),
    ("Микрофон занят проверкой в настройках голоса", "The microphone is busy with the check in the voice settings"),
    ("Не удалось открыть микрофон", "Could not open the microphone"),
    ("Голосовое сообщение слишком короткое", "The voice message is too short"),
    ("Discord не дал места для файла", "Discord did not give a place for the file"),
    // Hotkeys.
    ("Не задана", "Not set"),
    ("Пробел", "Space"),
    ("Стрелка влево", "Left arrow"),
    ("Стрелка вверх", "Up arrow"),
    ("Стрелка вправо", "Right arrow"),
    ("Стрелка вниз", "Down arrow"),
    ("Клавиша {}", "Key {}"),
    // Typing, times and sizes.
    ("{} печатает…", "{} is typing…"),
    ("{} и {} печатают…", "{} and {} are typing…"),
    ("{}, {} и {} печатают…", "{}, {} and {} are typing…"),
    ("Несколько человек печатают…", "Several people are typing…"),
    ("Сегодня, {}", "Today, {}"),
    ("Вчера, {}", "Yesterday, {}"),
    ("сейчас", "now"),
    ("{} мин", "{} min"),
    ("{} ч", "{} h"),
    ("{} дн", "{} d"),
    ("{} мес", "{} mo"),
    ("{} г", "{} y"),
    ("через {}", "in {}"),
    ("{} назад", "{} ago"),
    ("только что", "just now"),
    ("{} мин. назад", "{} min ago"),
    ("{} ч. назад", "{} h ago"),
    ("{} д. назад", "{} d ago"),
    ("ГБ", "GB"),
    ("МБ", "MB"),
    ("КБ", "KB"),
    ("{} Б", "{} B"),
    // Discord's answers.
    ("файл «{}» весит {}, а можно не больше {}", "the file \"{}\" is {}, but at most {} is allowed"),
    ("Нет связи: {}", "No connection: {}"),
    ("Вход устарел: войди в Nimbus заново", "The sign-in has expired: sign in to Nimbus again"),
    ("Нет доступа к этому действию", "You have no access to this action"),
    ("Не найдено: возможно, уже удалено", "Not found: it may have been deleted already"),
    ("Слишком часто, подожди секунду и повтори", "Too often, wait a second and try again"),
    ("Discord сейчас не отвечает, попробуй позже", "Discord is not answering right now, try later"),
    ("Discord не принял запрос", "Discord did not accept the request"),
    ("Не получилось: Discord отказал", "Did not work: Discord refused"),
    ("Нет связи с Discord: проверь интернет", "No connection to Discord: check the internet"),
    ("Discord не открыл переписку", "Discord did not open the conversation"),
    ("Discord не принял токен: {}", "Discord did not accept the token: {}"),
    ("Discord больше не принимает вход (аккаунт вышел или сменил пароль). Войди заново.", "Discord no longer accepts the sign-in (the account signed out or changed its password). Sign in again."),
    ("Не удалось войти: {}", "Could not sign in: {}"),
    ("Ошибка: {}", "Error: {}"),
    ("Не получилось: {}", "Did not work: {}"),
    // People.
    ("Не активен", "Idle"),
    ("Не беспокоить", "Do not disturb"),
    ("Не в сети", "Offline"),
    ("Участники", "Members"),
    ("Стримит {}", "Streaming {}"),
    ("Слушает {}", "Listening to {}"),
    ("Смотрит {}", "Watching {}"),
    ("Соревнуется в {}", "Competing in {}"),
    ("{} участник", "{} member"),
    ("{} участника", "{} members"),
    ("{} участников", "{} members"),
    ("Группа {}", "{}'s group"),
    ("Возможно, спам", "Possibly spam"),
    ("Хочет написать вам", "Wants to message you"),
    ("Заявка отправлена: {}", "Friend request sent: {}"),
    ("Такого пользователя нет. Проверь имя пользователя (не отображаемое имя).", "There is no such user. Check the username (not the display name)."),
    ("Вы уже друзья.", "You are already friends."),
    ("Этот пользователь не принимает заявки в друзья.", "This user does not accept friend requests."),
    ("Нельзя отправить заявку самому себе.", "You can't send a friend request to yourself."),
    ("Discord просит подтвердить аккаунт: сделай это в официальном приложении.", "Discord asks to verify the account: do it in the official app."),
    ("Discord просит капчу: отправь заявку из официального приложения.", "Discord asks for a captcha: send the request from the official app."),
    ("ты", "you"),
    ("кто-то", "someone"),
    ("Кто-то", "Someone"),
    // Messages.
    ("пользователь", "user"),
    ("@роль", "@role"),
    ("Личное сообщение", "Direct message"),
    ("Версия {}", "Version {}"),
    ("Версия {}, {} из {} МБ", "Version {}, {} of {} MB"),
    ("Версия {}, {} МБ", "Version {}, {} MB"),
    ("Нет подключения к Discord", "No connection to Discord"),
    ("Починить", "Fix"),
    ("Диагностика подключения", "Connection check"),
    ("Исправить", "Repair"),
    ("Скопировать отчёт", "Copy report"),
    ("Закрыть", "Close"),
    ("Свой прокси, если он есть", "Your own proxy, if you have one"),
    ("Применить", "Apply"),
    ("Интернет", "Internet"),
    ("Адрес Discord", "Discord address"),
    ("Доступ к серверу", "Reaching the server"),
    ("Сайт Discord", "Discord website"),
    ("Постоянное подключение", "Live connection"),
    ("Проверяю подключение…", "Checking the connection…"),
    ("Проверяю {}…", "Trying {}…"),
    ("Ищу рабочий путь…", "Looking for a way that works…"),
    ("Сохранено. Перезапусти Nimbus.", "Saved. Restart Nimbus."),
    ("Прямое подключение работает, Nimbus подключится сам.", "The direct connection works, Nimbus will connect by itself."),
    ("Этот прокси работает, Nimbus подключится сам.", "This proxy works, Nimbus will connect by itself."),
    ("Этот прокси не подключился: {}", "This proxy did not connect: {}"),
    ("Рабочий путь не найден. Включи VPN на весь трафик или впиши адрес прокси и нажми «Применить».", "No working way found. Turn on a VPN for all traffic, or type a proxy address and press Apply."),
    ("Связь с Discord есть. Если что-то всё равно не работает, скопируй отчёт и отправь разработчику.", "Discord can be reached. If something still does not work, copy the report and send it to the developer."),
    ("Discord не прошёл проверку сертификата. Проверь дату и время на компьютере и отключи проверку HTTPS в антивирусе.", "The certificate check for Discord failed. Check the date and time on this PC and turn off HTTPS scanning in your antivirus."),
    ("Похоже, на компьютере нет интернета. Проверь подключение к сети.", "This PC seems to have no internet. Check your network."),
    ("Discord закрыт в этой сети. Нажми «Исправить»: Nimbus поищет рабочий прокси на этом компьютере.", "Discord is closed on this network. Press Repair and Nimbus will look for a working proxy on this PC."),
    ("Нужен адрес вида 127.0.0.1:1080 или socks5://127.0.0.1:1080, без логина и пароля", "An address like 127.0.0.1:1080 or socks5://127.0.0.1:1080 is needed, without a login and password"),
    ("Отправить эту диагностику разработчикам?", "Send this report to the developers?"),
    ("Уйдёт только этот текст, токена и данных аккаунта в нём нет. Сервер увидит твой IP-адрес, как любой сайт.", "Only this text is sent, with no token or account data. The server will see your IP address, as any website does."),
    ("Отправить", "Send"),
    ("Не отправлять", "Don't send"),
    ("Отправляю…", "Sending…"),
    ("Отправлено.", "Sent."),
    ("Отправлено. Номер отчёта: {}", "Sent. Report number: {}"),
    ("Не удалось отправить: {}", "Could not send: {}"),
    ("Запускаю текущую версию", "Starting the current version"),
    ("Nimbus перезапустится сам", "Nimbus will restart by itself"),
    ("Общие друзья: {} • Общие серверы: {}", "{} mutual friends • {} mutual servers"),
    ("Общие друзья: {}", "{} mutual friends"),
    ("Общие серверы: {}", "{} mutual servers"),
    ("Сохранить картинку", "Save picture"),
    ("Эту картинку нельзя сохранить отсюда", "This picture can't be saved from here"),
    ("Картинка сохранена", "Picture saved"),
    ("Упоминания", "Mentions"),
    ("Не удалось сохранить картинку", "Could not save the picture"),
    ("#канал", "#channel"),
    ("[файл] {}", "[file] {}"),
    ("вложение", "attachment"),
    ("[ссылка] {}", "[link] {}"),
    ("в ответ {}: {}", "replying to {}: {}"),
    ("добавил(а) участника в группу", "added someone to the group"),
    ("покинул(а) группу", "left the group"),
    ("добавил(а) {} в группу", "added {} to the group"),
    ("убрал(а) {} из группы", "removed {} from the group"),
    ("Владелец", "Owner"),
    ("убрал(а) участника из группы", "removed someone from the group"),
    ("начал(а) звонок", "started a call"),
    ("закрепил(а) сообщение", "pinned a message"),
    ("присоединился(-ась) к серверу", "joined the server"),
    ("забустил(а) сервер", "boosted the server"),
    ("системное сообщение", "system message"),
    ("🔒 Нет доступа к этому каналу", "🔒 No access to this channel"),
    ("Не удалось загрузить сообщения: {}", "Could not load the messages: {}"),
    ("Не удалось загрузить публикации: {}", "Could not load the posts: {}"),
    ("Здесь пока нет публикаций", "No posts here yet"),
    ("Написать в #{}", "Message #{}"),
    ("Написать {}", "Message {}"),
    ("канал не выбран", "no channel is chosen"),
    ("не на сервере", "not on a server"),
    ("Закреплённые сообщения", "Pinned messages"),
    ("Выбери файл для отправки", "Choose a file to send"),
    ("файл", "file"),
    ("Отправка «{}»: {}%", "Sending \"{}\": {}%"),
    ("Выбери картинку", "Choose a picture"),
    ("Картинки", "Pictures"),
    ("Новое сообщение", "New message"),
    ("Вложение", "Attachment"),
    ("Упоминаний: {}", "Mentions: {}"),
    ("Идёт звонок: {}", "Call in progress: {}"),
    ("Идёт звонок: {}, {} и ещё {}", "Call in progress: {}, {} and {} more"),
    // The window and updates.
    ("Открыть Nimbus", "Open Nimbus"),
    ("Выход", "Quit"),
    ("Вышла версия {}. Обновить сейчас?", "Version {} is out. Update now?"),
    ("На этом сервере нет канала для приглашения", "This server has no channel to invite to"),
    ("Начальный", "Starter"),
    ("Бронза", "Bronze"),
    ("Серебро", "Silver"),
    ("Золото", "Gold"),
    ("Платина", "Platinum"),
    ("Алмаз", "Diamond"),
    ("Изумруд", "Emerald"),
    ("Рубин", "Ruby"),
    ("Опал", "Opal"),
    ("1 месяц", "1 month"),
    ("2 месяца", "2 months"),
    ("3 месяца", "3 months"),
    ("6 месяцев", "6 months"),
    ("9 месяцев", "9 months"),
    ("12 месяцев", "12 months"),
    ("15 месяцев", "15 months"),
    ("18 месяцев", "18 months"),
    ("От 24 месяцев", "24+ months"),
    ("1 год", "1 year"),
    ("2 года", "2 years"),
    ("3 года", "3 years"),
    ("5 лет", "5 years"),
    ("От 6 лет", "6+ years"),
    ("Буст сервера", "Server boost"),
    ("Код подтверждения", "Verification code"),
    ("Код подтверждения пока недоступен", "The verification code isn't available yet"),
    ("Аккаунт создан {}", "Account created {}"),
    ("Участник сервера с {}", "Server member since {}"),
    ("Никнейм: {}", "Nickname: {}"),
    ("Роли: {}", "Roles: {}"),
    ("Тайм-аут до {}", "Timed out until {}"),
    ("Доступ модератора", "Moderator view"),
    ("Избранное", "Favorites"),
    ("В Discord не подключён Spotify", "Spotify isn't linked in Discord"),
    ("Spotify не дал доступ", "Spotify gave no access"),
    ("Spotify не отвечает", "Spotify isn't answering"),
    ("Откройте Spotify на любом устройстве", "Open Spotify on any device"),
    ("Для этого нужен Spotify Premium", "This needs Spotify Premium"),
    ("Spotify ответил {}", "Spotify answered {}"),
    ("Приглашение отправлено", "Invite sent"),
    ("Вход через discord.com", "Sign in with discord.com"),
    ("не удалось открыть окно входа: {}", "could not open the sign-in window: {}"),
    ("окно входа закрыто без входа", "the sign-in window was closed without signing in"),
    // Voice servers.
    ("адрес «{}» должен начинаться с http:// или https://", "the address \"{}\" must start with http:// or https://"),
    ("в адресе «{}» не получилось найти сервер", "could not find a server in the address \"{}\""),
    ("в адресе «{}» не закрыта ]", "the ] is not closed in the address \"{}\""),
    ("в адресе «{}» неверный порт «{}»", "the address \"{}\" has a wrong port \"{}\""),
    ("в адресе «{}» нет имени сервера", "the address \"{}\" has no server name"),
    ("Сервер {} стоит в интернете, но без шифрования (http). Ему нужен https: через Caddy, Cloudflare Tunnel или Tailscale (см. server\\README.md).", "The server {} is on the internet but without encryption (http). It needs https: through Caddy, Cloudflare Tunnel or Tailscale (see server\\README.md)."),
    ("сервер вернул странный адрес голоса «{}»", "the server returned a strange voice address \"{}\""),
    ("Сервер {} отдал голос без шифрования (ws://{}). Нужен wss://: настрой https для LiveKit (server\\README.md).", "The server {} gave voice without encryption (ws://{}). wss:// is needed: set up https for LiveKit (server\\README.md)."),
    ("Ты отказался подключаться к голосовому серверу этого сообщества ({}). Передумать можно: Настройки → Система → «Забыть решения».", "You refused to connect to this community's voice server ({}). To change your mind: Settings → System → \"Forget decisions\"."),
    ("сервер голоса недоступен ({}): {}", "the voice server is unreachable ({}): {}"),
    ("сервер голоса ответил {}", "the voice server answered {}"),
    ("сервер голоса не прислал адрес", "the voice server did not send an address"),
    ("сервер голоса не прислал токен", "the voice server did not send a token"),
    // Voice and streams.
    ("устройство пропало", "the device is gone"),
    ("Нет доступа к микрофону и динамикам", "No access to the microphone and speakers"),
    ("Не удалось сменить микрофон: {}", "Could not switch the microphone: {}"),
    ("Не удалось сменить устройство вывода: {}", "Could not switch the output device: {}"),
    ("не удалось подключиться к серверу голоса: {}", "could not connect to the voice server: {}"),
    ("не удалось включить микрофон: {}", "could not turn on the microphone: {}"),
    ("Голос отключился: {}", "Voice disconnected: {}"),
    (" · Discord {} мс", " · Discord {} ms"),
    (" · сервер {} мс", " · server {} ms"),
    (" · потери {}.{}%", " · loss {}.{}%"),
    ("Чтобы показать экран, сначала зайди в голосовой канал", "To share your screen, join a voice channel first"),
    ("Чтобы показать на Discord, зайди в голосовой канал Discord", "To share on Discord, join a Discord voice channel"),
    ("Сервер Nimbus не подключён: показ экрана доступен только на Discord", "The Nimbus server is not connected: screen sharing is only available on Discord"),
    ("Для показа на Discord нужен аппаратный кодировщик H.264 (NVIDIA, AMD или Intel)", "Sharing on Discord needs a hardware H.264 encoder (NVIDIA, AMD or Intel)"),
    ("не удалось начать показ экрана: {}", "could not start the screen share: {}"),
    ("Показ на Discord: {}", "Sharing on Discord: {}"),
    ("Не удалось включить звук демки: {}", "Could not turn on the share's sound: {}"),
    ("Показ можно открыть, когда голос подключён: подожди секунду или зайди в канал заново", "A share can be opened once voice is connected: wait a second or rejoin the channel"),
    ("Твоя демка", "Your share"),
    ("Стрим Discord", "Discord stream"),
    ("Стрим Discord · {}", "Discord stream · {}"),
    ("Стрим Discord можно открыть, когда голос Discord подключён", "A Discord stream can be opened once Discord voice is connected"),
    ("{}P {} КАДРОВ В СЕКУНДУ", "{}P {} FPS"),
    ("подключение…", "connecting…"),
    ("ошибка: {}", "error: {}"),
    ("жду ключевой кадр…", "waiting for a keyframe…"),
    ("кадров {}, потеряно {}", "{} frames, {} lost"),
    ("подключено, видео {}", "connected, video {}"),
    ("стрим закрыт", "stream closed"),
    ("не нашёл монитор: {}", "could not find the monitor: {}"),
    ("окно закрыто или свёрнуто", "the window is closed or minimised"),
    ("основной", "main"),
    ("основной · {}", "main · {}"),
    ("Экран {} · {}", "Screen {} · {}"),
    ("программный (WebRTC)", "software (WebRTC)"),
    ("камера не запустилась", "the camera did not start"),
    ("камера не найдена", "no camera found"),
    ("камера не даёт подходящего видео", "the camera offers no usable video"),
    ("камера отключилась", "the camera was disconnected"),
    ("Камера не найдена", "No camera found"),
    ("Камера работает в голосе Discord: зайди в голосовой канал", "The camera works in Discord voice: join a voice channel"),
    ("Не удалось сменить камеру: {}", "Could not switch the camera: {}"),
    ("Не удалось включить камеру: {}", "Could not turn the camera on: {}"),
    ("Для камеры нужен аппаратный кодировщик H.264 (NVIDIA, AMD или Intel)", "The camera needs a hardware H.264 encoder (NVIDIA, AMD or Intel)"),
    ("Часто используемые", "Frequently used"),
    ("Команда не сработала: нет связи с Discord", "The command didn't work: no connection to Discord"),
    ("Команда не сработала: {}", "The command didn't work: {}"),
    ("Не хватает параметра «{}»", "The option \"{}\" is missing"),
    ("Неверное значение параметра «{}»", "Wrong value for the option \"{}\""),
    ("Параметр «{}» здесь не задать", "The option \"{}\" can't be given here"),
    ("Лишний текст: {}", "Extra text: {}"),
    ("Недавние", "Recent"),
    ("Этот сервер", "This server"),
    ("Стандартные", "Default"),
    ("Звук не проиграть: {}", "Could not play the sound: {}"),
    // Bots' buttons and menus, pasted files.
    ("Взаимодействие не удалось", "Interaction failed"),
    ("Файл «{}» больше {}", "The file \"{}\" is bigger than {}"),
    ("Не больше {} файлов за раз", "No more than {} files at once"),
    ("Отправка файлов…", "Sending files…"),
    // Scheduled events.
    ("Мероприятия", "Events"),
    ("{} событие", "{} event"),
    ("{} события", "{} events"),
    ("{} событий", "{} events"),
    ("Сегодня, в {}", "Today at {}"),
    ("Завтра, в {}", "Tomorrow at {}"),
    ("{}, в {}", "{} at {}"),
    ("Укажите тему мероприятия (до 100 символов)", "Enter the event topic (up to 100 characters)"),
    ("Описание не длиннее 1000 символов", "The description can't be longer than 1000 characters"),
    ("Дата и время начала: дд.мм.гггг и чч:мм", "Start date and time: dd.mm.yyyy and hh:mm"),
    ("Время начала уже прошло", "The start time has already passed"),
    ("Выберите канал", "Choose a channel"),
    ("Укажите место (до 100 символов)", "Enter a location (up to 100 characters)"),
    ("Дата и время окончания: дд.мм.гггг и чч:мм", "End date and time: dd.mm.yyyy and hh:mm"),
    ("Окончание должно быть позже начала", "The end must be after the start"),
    ("Выберите, где пройдёт мероприятие", "Choose where the event takes place"),
];

/// Chosen in the settings (and at start); applies to texts made afterwards.
pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

fn lookup(ru: &str, english: bool) -> &str {
    if english {
        if let Some((_, en)) = TABLE.iter().find(|(k, _)| *k == ru) {
            return en;
        }
    }
    ru
}

/// `{}` in the text filled with `args` in order; extra holes stay empty, extra args are dropped.
fn fill(template: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut parts = template.split("{}");
    out.push_str(parts.next().unwrap_or(""));
    for part in parts {
        if let Some(a) = args.next() {
            out.push_str(&a.to_string());
        }
        out.push_str(part);
    }
    out
}

/// The text in the chosen language.
/// The texts in the other languages, by their English: English, then one column per language of `OTHERS`.
/// What isn't here stays English.
const OTHERS: [&str; 7] = ["uk", "es", "de", "fr", "pt", "pl", "tr"];
const TRANSLATIONS: &str = include_str!("../lang/translations.txt");
/// Which column of the translations is in use; 0: none.
static COLUMN: AtomicUsize = AtomicUsize::new(0);

/// Chooses the language; other than Russian, everything starts from the English text.
pub fn set_language(code: &str) {
    // Ukrainian counts as Russian does, so it has the same three forms of a number's word.
    set_english(!matches!(code, "ru" | "uk"));
    COLUMN.store(OTHERS.iter().position(|c| *c == code).map_or(0, |at| at + 1), Ordering::Relaxed);
}


fn in_column(en: &str, column: usize) -> &str {
    if column == 0 {
        return en;
    }
    static ROWS: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
        TRANSLATIONS
            .lines()
            .filter_map(|line| {
                let cells: Vec<&str> = line.split('|').collect();
                Some((*cells.first()?, cells))
            })
            .collect()
    });
    ROWS.get(en).and_then(|cells| cells.get(column)).copied().unwrap_or(en)
}

/// The texts made in Rust in the other languages, by their Russian: one tab-separated column per language of `OTHERS`.
const RUST_TEXTS: &str = include_str!("../lang/rust.tsv");

fn rust_text(ru: &str, column: usize) -> Option<&'static str> {
    static ROWS: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
        RUST_TEXTS
            .lines()
            .filter_map(|line| {
                let cells: Vec<&str> = line.split('\t').collect();
                Some((*cells.first()?, cells))
            })
            .collect()
    });
    ROWS.get(ru).and_then(|cells| cells.get(column)).copied()
}

fn shown(ru: &str) -> &str {
    let column = COLUMN.load(Ordering::Relaxed);
    if column == 0 {
        return lookup(ru, english());
    }
    // A text newer than the translations goes through its English.
    rust_text(ru, column).unwrap_or_else(|| in_column(lookup(ru, true), column))
}

pub fn t(ru: &str) -> String {
    shown(ru).to_string()
}

/// A text with `{}` holes ("Не удалось войти: {}"): the template is translated, then filled.
pub fn tf(ru: &str, args: &[&dyn std::fmt::Display]) -> String {
    fill(shown(ru), args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_no_empty_or_repeated_entries() {
        for (i, (ru, en)) in TABLE.iter().enumerate() {
            assert!(!ru.is_empty() && !en.is_empty());
            assert!(TABLE.iter().skip(i + 1).all(|(k, _)| k != ru), "{ru} twice");
            assert_eq!(ru.matches("{}").count(), en.matches("{}").count(), "holes of {ru}");
        }
    }

    #[test]
    fn english_swaps_known_texts_and_keeps_others() {
        assert_eq!(lookup("Друзья", true), "Friends");
        assert_eq!(lookup("что-то новое", true), "что-то новое");
        assert_eq!(lookup("Друзья", false), "Друзья");
    }

    #[test]
    fn every_translation_has_all_its_columns_and_holes() {
        for line in TRANSLATIONS.lines() {
            let cells: Vec<&str> = line.split('|').collect();
            assert_eq!(cells.len(), OTHERS.len() + 1, "{line}");
            for cell in &cells {
                assert!(!cell.trim().is_empty(), "{line}");
                assert_eq!(cell.matches("{}").count(), cells[0].matches("{}").count(), "{line}");
            }
        }
        assert_eq!(in_column("Friends", 1), "Друзі");
        assert_eq!(in_column("Friends", 7), "Arkadaşlar");
        assert_eq!(in_column("Something else", 1), "Something else", "stays English");
        assert_eq!(in_column("Friends", 0), "Friends");
    }

    #[test]
    fn rust_texts_line_up_with_the_table() {
        for line in RUST_TEXTS.lines() {
            let cells: Vec<&str> = line.split('\t').collect();
            assert_eq!(cells.len(), OTHERS.len() + 1, "{line}");
            for cell in &cells {
                assert_eq!(cell.matches("{}").count(), cells[0].matches("{}").count(), "{line}");
            }
        }
        assert_eq!(rust_text("Друзья", 1), Some("Друзі"));
        assert_eq!(rust_text("Друзья", 7), Some("Arkadaşlar"));
        assert_eq!(rust_text("нет такого текста", 1), None);
    }

    #[test]
    fn templates_fill_in_order() {
        assert_eq!(fill(lookup("Не удалось войти: {}", true), &[&"нет сети"]), "Could not sign in: нет сети");
        assert_eq!(fill("{} и {} печатают…", &[&"Аня", &2]), "Аня и 2 печатают…");
        assert_eq!(fill("{} мин", &[]), " мин");
    }

    /// Russian texts in the code that reach the window must go through `t`/`tf` (and the table).
    /// A rough scan: string literals with Cyrillic, outside comments, tests and the demo, that are
    /// not the first argument of `t(`/`tf(`; logs and search words are allowed by name below.
    #[test]
    fn russian_texts_go_through_the_table() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut missing = Vec::new();
        let mut untranslated = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            // The table itself; emoji search words (typed, never shown).
            if !file.ends_with(".rs") || file == "i18n.rs" || file == "emoji.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
            for lit in scan::literals(&text) {
                if !lit.text.chars().any(scan::cyrillic) || lit.skipped {
                    continue;
                }
                if lit.translated {
                    if lookup(&lit.text, true) == lit.text {
                        untranslated.push(format!("{file}:{}: {}", lit.line, lit.text));
                    }
                } else if !ALLOWED.iter().any(|a| lit.text.contains(a)) {
                    missing.push(format!("{file}:{}: {}", lit.line, lit.text));
                }
            }
        }
        assert!(missing.is_empty(), "Russian texts not passed through i18n::t/tf:\n{}", missing.join("\n"));
        assert!(untranslated.is_empty(), "no English in the table for:\n{}", untranslated.join("\n"));
    }

    /// Russian literals that never reach the window, only the log: the Go Live sender's progress,
    /// the hardware encoder's errors (a software one takes over), the voice server's keys and the
    /// update check's errors.
    const ALLOWED: &[&str] = &[
        "запрашиваю стрим…",
        "в эфире · кадров {}",
        "подключено, жду ключевой кадр",
        "аппаратное кодирование выключено",
        "поток кодировщика не запустился",
        "аппаратный кодировщик",
        "пустой кодировщик",
        "Windows не дал случайные числа",
        "keys.json",
        "latest.json",
        "подпись обновления неверна",
        "сервер обновлений ответил",
        "установщик",
    ];

    /// Just enough of Rust's syntax to find string literals and where they stand.
    mod scan {
        pub fn cyrillic(c: char) -> bool {
            matches!(c, 'А'..='я' | 'Ё' | 'ё')
        }

        pub struct Literal {
            pub line: usize,
            pub text: String,
            /// The first argument of `t(` or `tf(`.
            pub translated: bool,
            /// In a `mod tests`-like module or a function with "demo" in its name, or a log line.
            pub skipped: bool,
        }

        pub fn literals(src: &str) -> Vec<Literal> {
            let b: Vec<char> = src.chars().collect();
            let (mut i, mut line, mut depth) = (0, 1, 0usize);
            // (brace depth, skipped) of named items; `pending` waits for the item's `{`.
            let mut items: Vec<(usize, bool)> = Vec::new();
            let mut pending: Option<bool> = None;
            let mut out = Vec::new();
            let ident = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
            let starts = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| b.get(i + k) == Some(&c));
            while i < b.len() {
                let c = b[i];
                if c == '\n' {
                    line += 1;
                } else if starts(i, "//") {
                    while i < b.len() && b[i] != '\n' {
                        i += 1;
                    }
                    continue;
                } else if starts(i, "/*") {
                    while i < b.len() && !starts(i, "*/") {
                        line += (b[i] == '\n') as usize;
                        i += 1;
                    }
                    i += 2;
                    continue;
                } else if c == '"' || (c == 'r' && !ident(i.checked_sub(1).and_then(|p| b.get(p))) && matches!(b.get(i + 1), Some('"' | '#'))) {
                    let (start_line, at) = (line, i);
                    let mut text = String::new();
                    if c == 'r' {
                        let mut hashes = 0;
                        i += 1;
                        while b[i] == '#' {
                            hashes += 1;
                            i += 1;
                        }
                        i += 1;
                        let close: String = std::iter::once('"').chain(std::iter::repeat('#').take(hashes)).collect();
                        while !starts(i, &close) {
                            line += (b[i] == '\n') as usize;
                            text.push(b[i]);
                            i += 1;
                        }
                        i += close.chars().count();
                    } else {
                        i += 1;
                        while b[i] != '"' {
                            line += (b[i] == '\n') as usize;
                            if b[i] == '\\' {
                                i += 1;
                                match b[i] {
                                    'n' => text.push('\n'),
                                    // A line break escaped away, with the next line's indent.
                                    '\n' => {
                                        line += 1;
                                        while b[i + 1].is_whitespace() {
                                            line += (b[i + 1] == '\n') as usize;
                                            i += 1;
                                        }
                                    }
                                    other => text.push(other),
                                }
                            } else {
                                text.push(b[i]);
                            }
                            i += 1;
                        }
                        i += 1;
                    }
                    let before: String = b[..at].iter().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
                    let before = before.trim_end();
                    let call = before.strip_suffix("t(").or_else(|| before.strip_suffix("tf("));
                    let translated = call.is_some_and(|c| !c.ends_with(|c: char| c.is_alphanumeric() || c == '_'));
                    let skipped = items.iter().any(|(_, s)| *s) || logged(before);
                    out.push(Literal { line: start_line, text, translated, skipped });
                    continue;
                } else if c == '\'' {
                    // A char literal ('a', '\n', '«'); a lifetime has no closing quote nearby.
                    let close = if b.get(i + 1) == Some(&'\\') { (i + 3..i + 12).find(|&k| b.get(k) == Some(&'\'')) } else { (b.get(i + 2) == Some(&'\'')).then_some(i + 2) };
                    i = close.map_or(i + 1, |k| k + 1);
                    continue;
                } else if !ident(i.checked_sub(1).and_then(|p| b.get(p))) && (starts(i, "fn ") || starts(i, "mod ")) {
                    let name: String = b[i..].iter().skip_while(|c| !c.is_whitespace()).skip_while(|c| c.is_whitespace()).take_while(|c| c.is_alphanumeric() || **c == '_').collect();
                    pending = Some(name.contains("demo") || (starts(i, "mod ") && name.contains("test")));
                } else if c == ';' {
                    pending = None;
                } else if c == '{' {
                    depth += 1;
                    if let Some(skip) = pending.take() {
                        items.push((depth, skip));
                    }
                } else if c == '}' {
                    if items.last().is_some_and(|(d, _)| *d == depth) {
                        items.pop();
                    }
                    depth = depth.saturating_sub(1);
                }
                i += 1;
            }
            out
        }

        /// The literal is (part of) a log line's text.
        fn logged(before: &str) -> bool {
            let statement = before.rsplit([';', '{', '}']).next().unwrap_or("");
            statement.contains("log_line(") || statement.contains("println!(")
        }
    }
}
