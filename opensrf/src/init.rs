use super::conf;
use super::logging;
use std::env;

const DEFAULT_OSRF_CONFIG: &str = "/openils/conf/opensrf_core.xml";

pub struct InitOptions {
    pub skip_logging: bool,
    // Application name to use with syslog.
    pub appname: Option<String>,
}

impl InitOptions {
    pub fn new() -> InitOptions {
        InitOptions {
            skip_logging: false,
            appname: None,
        }
    }
}

/// Read environment variables, parse the core config, setup logging.
///
/// This does not connect to the bus.
pub fn init() -> Result<conf::Config, String> {
    init_with_options(&InitOptions::new())
}

pub fn init_with_options(options: &InitOptions) -> Result<conf::Config, String> {
    let builder = if let Ok(fname) = env::var("OSRF_CONFIG") {
        conf::ConfigBuilder::from_file(&fname)?
    } else {
        conf::ConfigBuilder::from_file(DEFAULT_OSRF_CONFIG)?
    };

    let mut config = builder.build()?;

    if let Ok(_) = env::var("OSRF_LOCALHOST") {
        config.set_hostname("localhost");
    } else if let Ok(v) = env::var("OSRF_HOSTNAME") {
        config.set_hostname(&v);
    }

    // When custom client connection/logging values are provided via
    // the ENV, propagate them to all variations of a client connection
    // supported by the current opensrf_core.xml format.

    if let Ok(level) = env::var("OSRF_LOG_LEVEL") {
        config.client_mut().logging_mut().set_log_level(&level);

        if let Some(gateway) = config.gateway_mut() {
            gateway.logging_mut().set_log_level(&level);
            // Copy the requested log leve into the gateway config.
        }

        for router in config.routers_mut() {
            router.client_mut().logging_mut().set_log_level(&level);
        }
    }

    if let Ok(username) = env::var("OSRF_BUS_USERNAME") {
        config.client_mut().set_username(&username);

        if let Some(gateway) = config.gateway_mut() {
            gateway.set_username(&username);
            // Copy the requested log leve into the gateway config.
        }

        for router in config.routers_mut() {
            router.client_mut().set_username(&username);
        }
    }

    if let Ok(password) = env::var("OSRF_BUS_PASSWORD") {
        config.client_mut().set_password(&password);

        if let Some(gateway) = config.gateway_mut() {
            gateway.set_password(&password);
            // Copy the requested log leve into the gateway config.
        }

        for router in config.routers_mut() {
            router.client_mut().set_password(&password);
        }
    }

    if !options.skip_logging {
        let mut logger = logging::Logger::new(config.client().logging())?;
        if let Some(name) = options.appname.as_ref() {
            logger.set_application(name);
        }
        logger
            .init()
            .or_else(|e| Err(format!("Error initializing logger: {e}")))?;
    }

    Ok(config)
}
