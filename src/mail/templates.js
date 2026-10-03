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
            `The link is valid for ${hours} hours. If you did not sign up, ignore this message:\n` +
            'without the confirmation nothing is created or confirmed.\n' + sign(serverName),
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
 * Sent to the owner of an address somebody tried to register again, or (emailChange) that another
 * player asked to move their account to.
 * @param {{ serverName: string, username: string, emailChange?: boolean }} v
 */
export function registrationAttempt({ serverName, username, emailChange = false }) {
    if (emailChange) {
        return {
            subject: `Someone tried to use your e-mail address on ${serverName}`,
            text: `Hello ${username},\n\n` +
                `A player of ${serverName} asked to change the e-mail address of their account to yours.\n` +
                'Your address already belongs to your account, so it was not given to theirs, and your\n' +
                'account did not change.\n\n' +
                'You do not need to do anything.\n' + sign(serverName),
        };
    }
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

/**
 * Sent to the new address of an e-mail change: the confirmation link.
 * @param {{ serverName: string, username: string, link: string, hours: number }} v
 */
export function emailChangeConfirm({ serverName, username, link, hours }) {
    return {
        subject: `Confirm your new e-mail address for ${serverName}`,
        text: `Hello ${username},\n\n` +
            `You asked to use this e-mail address for your ${serverName} account. To confirm it, open\n` +
            `this link and press the confirmation button:\n\n${link}\n\n` +
            `The link is valid for ${hours} hours and can be used once. Until it is used, your account\n` +
            'keeps its current address.\n\n' +
            'If you did not ask for this, ignore this message: nothing changes.\n' + sign(serverName),
    };
}

/**
 * Sent to the current address when a change of address is requested.
 * @param {{ serverName: string, username: string, maskedEmail: string, when: Date, hours: number }} v
 */
export function emailChangeRequested({ serverName, username, maskedEmail, when, hours }) {
    return {
        subject: `A change of your ${serverName} e-mail address was requested`,
        text: `Hello ${username},\n\n` +
            `On ${when.toUTCString()}, a change of the e-mail address of your ${serverName} account\n` +
            `to ${maskedEmail} was requested, with your password. The address changes only if the\n` +
            `link sent to the new address is opened within ${hours} hours; until then, this address stays\n` +
            'the one of your account.\n\n' +
            'If it was you, there is nothing else to do.\n\n' +
            'If it was not you, someone knows your password: change it at once in the game, or reset it\n' +
            'with "Forgot password". A new password cancels the change of address.\n' + sign(serverName),
    };
}

/**
 * Sent to the former address once the change of address is done.
 * @param {{ serverName: string, username: string, maskedEmail: string, when: Date }} v
 */
export function emailChanged({ serverName, username, maskedEmail, when }) {
    return {
        subject: `Your ${serverName} e-mail address was changed`,
        text: `Hello ${username},\n\n` +
            `The e-mail address of your ${serverName} account was changed to ${maskedEmail} on\n` +
            `${when.toUTCString()}.\n\n` +
            'Messages about your account, password resets included, now go to the new address: this\n' +
            'is the last one sent to this address.\n\n' +
            `If you did not do this, someone else controls your account: contact the administrator of\n` +
            `${serverName} at once.\n` + sign(serverName),
    };
}

/**
 * Sent to the address of an account that Google sign-in just created.
 * @param {{ serverName: string, username: string, when: Date }} v
 */
export function ssoAccountCreated({ serverName, username, when }) {
    return {
        subject: `A ${serverName} account was created with your Google account`,
        text: `Hello ${username},\n\n` +
            `The ${serverName} account "${username}" was created with Google sign-in, with the Google\n` +
            `account of this address, on ${when.toUTCString()}.\n\n` +
            'If it was you, there is nothing else to do.\n\n' +
            'If it was not you, someone else may be signed in to it: sign in with Google in the game,\n' +
            `use "Sign out everywhere" on the account page, and contact the administrator of ${serverName}.\n` +
            'Never send anyone the address your browser shows after a sign-in.\n' + sign(serverName),
    };
}

/**
 * Sent to the account's address when Google sign-in is added to an existing account (after its
 * password, and its second factor when on).
 * @param {{ serverName: string, username: string, when: Date }} v
 */
export function ssoLinked({ serverName, username, when }) {
    return {
        subject: `Google sign-in was added to your ${serverName} account`,
        text: `Hello ${username},\n\n` +
            `Google sign-in was added to your ${serverName} account "${username}" on\n` +
            `${when.toUTCString()}, with your password. From now on, the Google account of this address\n` +
            'signs in to it without the password.\n\n' +
            'If it was you, there is nothing else to do.\n\n' +
            'If it was not you, someone knows your password: change it at once in the game, or reset it\n' +
            'with "Forgot password", then use "Sign out everywhere" on the account page and contact the\n' +
            `administrator of ${serverName}.\n` + sign(serverName),
    };
}

export const templates = {
    verification, passwordReset, registrationAttempt, mfaDisabled, passwordChanged, emailChangeConfirm, emailChangeRequested,
    emailChanged, ssoAccountCreated, ssoLinked,
};
