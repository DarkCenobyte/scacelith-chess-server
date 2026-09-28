// Plain-text English e-mail templates. Each returns { subject, text }. They never contain a
// password or a session token; verification and reset links carry single-use tokens that expire.

function sign(serverName) {
    return `\n-- \n${serverName}\nThis is an automatic message; replies are not read.\n`;
}

/**
 * @param {{ serverName: string, username: string, link: string, hours: number }} v
 */
export function verification({ serverName, username, link, hours }) {
    return {
        subject: `Confirm your e-mail address for ${serverName}`,
        text: `Hello ${username},\n\n` +
            `Welcome to ${serverName}. To confirm your e-mail address and start playing online, open\n` +
            `this link and press the confirmation button:\n\n${link}\n\n` +
            `The link is valid for ${hours} hours. If you did not create this account, ignore this\n` +
            'message: without the confirmation the account cannot be used.\n' + sign(serverName),
    };
}

/**
 * @param {{ serverName: string, username: string, link: string, minutes: number }} v
 */
export function passwordReset({ serverName, username, link, minutes }) {
    return {
        subject: `Reset your ${serverName} password`,
        text: `Hello ${username},\n\n` +
            `Someone (hopefully you) asked to reset the password of your ${serverName} account.\n` +
            `To choose a new password, open this link:\n\n${link}\n\n` +
            `The link is valid for ${minutes} minutes and can be used once. Resetting the password\n` +
            'signs out every device. Two-step verification, if enabled, stays enabled.\n\n' +
            'If you did not ask for this, ignore this message: your password does not change.\n' + sign(serverName),
    };
}

/**
 * Sent to the owner of an address somebody tried to register again.
 * @param {{ serverName: string, username: string, forgotHint: boolean }} v
 */
export function registrationAttempt({ serverName, username }) {
    return {
        subject: `Someone tried to register on ${serverName} with your e-mail address`,
        text: `Hello ${username},\n\n` +
            `Someone tried to create a new ${serverName} account with your e-mail address. Your\n` +
            'address already belongs to your account, so no new account was created.\n\n' +
            'If it was you, you can simply log in. If you forgot your password, use "Forgot\n' +
            'password" in the game to receive a reset link.\n\n' +
            'If it was not you, you do not need to do anything.\n' + sign(serverName),
    };
}

/**
 * @param {{ serverName: string, username: string, when: Date }} v
 */
export function mfaDisabled({ serverName, username, when }) {
    return {
        subject: `Two-step verification was turned off on ${serverName}`,
        text: `Hello ${username},\n\n` +
            `Two-step verification (authenticator codes) was turned off for your ${serverName}\n` +
            `account on ${when.toUTCString()}.\n\n` +
            'If you did not do this, reset your password at once from the game ("Forgot password"),\n' +
            'then turn two-step verification on again.\n' + sign(serverName),
    };
}

/**
 * @param {{ serverName: string, username: string, when: Date, byReset: boolean }} v
 */
export function passwordChanged({ serverName, username, when, byReset }) {
    return {
        subject: `Your ${serverName} password was changed`,
        text: `Hello ${username},\n\n` +
            `The password of your ${serverName} account was ${byReset ? 'reset with an e-mail link' : 'changed'} on\n` +
            `${when.toUTCString()}. ${byReset ? 'Every device was signed out.' : 'Your other devices were signed out.'}\n\n` +
            'If you did not do this, reset your password at once from the game ("Forgot password").\n' + sign(serverName),
    };
}

export const templates = { verification, passwordReset, registrationAttempt, mfaDisabled, passwordChanged };
