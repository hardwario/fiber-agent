-- An installation that already sends e-mail keeps English (plan/decisions.md #36).
--
-- V18 gave `email_lang` the default 'cs', and the default landed on the one
-- row every installation already has, so an upgrade switched v1's English
-- e-mails, subject tags included, to Czech without anyone choosing it. Where
-- e-mail is set up (`provider` is not 'none') the language goes back to 'en';
-- an installation without e-mail, and a new one, keep 'cs'.
UPDATE notify_global_config SET email_lang = 'en' WHERE provider != 'none';
